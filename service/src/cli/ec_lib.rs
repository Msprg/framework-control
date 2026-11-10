//! In-process EC access through `framework_lib`, the library behind `framework_tool`.
//!
//! Spawning `framework_tool` costs ~100 ms of process startup per call on Windows, and the fan
//! curve and telemetry tasks need a thermal reading about once a second. This module talks to the
//! EC driver directly and is used in front of the CLI: [`FrameworkTool`](super::FrameworkTool)
//! tries it first and falls back to spawning `framework_tool` whenever it can't answer.
//!
//! `framework_lib` 0.6.6 was written for a short-lived CLI process, so a few things need care when
//! it lives inside the service:
//!
//! - **One owner thread.** Every EC call runs on a single dedicated thread, in order. The library is
//!   synchronous, and its Linux port-I/O driver relies on per-thread `ioperm` permissions, so calls
//!   from arbitrary threads could crash the process. Port I/O is never used here anyway: only the
//!   Windows CrosEC driver and the Linux `/dev/cros_ec` driver are opened; otherwise the CLI is used.
//! - **Panics.** The Windows and Linux drivers `unwrap()` failed ioctls, in places while holding a
//!   global lock. A panic there poisons the lock and every later call panics too. Each call is
//!   wrapped in `catch_unwind`; after a panic a cheap probe checks whether the EC still answers. If
//!   it doesn't (or panics keep happening), the backend stops for the rest of the process lifetime
//!   and everything goes through the CLI, which gets a fresh process each time.
//! - **Hangs.** A call that overruns its deadline marks the backend busy instead of dead (a laptop
//!   may simply have been asleep mid-call). Callers use the CLI until the stuck call returns, and
//!   calls that were queued behind it are dropped rather than executed late.
//! - **Output parity.** Results are shaped exactly like the parsed CLI output (sensor/fan names,
//!   which readings are present), so saved fan curves keep matching.
//!
//! Set `FRAMEWORK_CONTROL_EC_BACKEND=cli` to disable this module and always spawn the CLI.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

pub use platform::Ctx;

/// Why the in-process backend could not answer a call. Every variant means "use the CLI instead".
#[derive(Debug, Clone, PartialEq)]
pub enum LibError {
    /// Not started, disabled, unavailable on this machine, or stopped after a failure.
    Unavailable,
    /// A previous call overran its deadline and hasn't returned yet.
    Busy,
    /// The EC (or the library) reported an error for this call; the backend stays usable.
    Ec(String),
    /// The library panicked during this call.
    Panicked(String),
}

impl std::fmt::Display for LibError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibError::Unavailable => write!(f, "in-process EC backend unavailable"),
            LibError::Busy => write!(f, "in-process EC backend busy"),
            LibError::Ec(e) => write!(f, "{e}"),
            LibError::Panicked(e) => write!(f, "framework_lib panicked: {e}"),
        }
    }
}

/// Context owned by the worker thread. `probe` must be cheap and must not mutate EC state.
pub trait Probe {
    fn probe(&mut self) -> bool;
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// How long a caller waits for a call before using the CLI and marking the worker busy.
    pub call_timeout: Duration,
    /// How long to wait for the worker to open the EC.
    pub init_timeout: Duration,
    /// First retry delay after the EC could not be opened; doubles up to `retry_max`.
    pub retry_initial: Duration,
    pub retry_max: Duration,
    /// Stop the backend after this many panics even if the EC keeps answering the probe.
    pub max_panics: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(10),
            init_timeout: Duration::from_secs(15),
            retry_initial: Duration::from_secs(30),
            retry_max: Duration::from_secs(600),
            max_panics: 3,
        }
    }
}

struct Job<C> {
    id: u64,
    run: Box<dyn FnOnce(&mut C) + Send>,
}

enum Slot<C> {
    Idle,
    Ready(mpsc::Sender<Job<C>>),
    Waiting {
        reason: String,
        retry_at: Instant,
        backoff: Duration,
    },
    Disabled(String),
}

#[derive(Default)]
struct Shared {
    dead: AtomicBool,
    dead_reason: Mutex<Option<String>>,
    panics: AtomicU32,
    next_id: AtomicU64,
    /// Highest job id that has finished (or was skipped) on the worker.
    last_done: AtomicU64,
    /// Highest job id whose caller gave up waiting. Busy while `stuck > last_done`.
    stuck: AtomicU64,
}

impl Shared {
    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    fn is_busy(&self) -> bool {
        self.stuck.load(Ordering::SeqCst) > self.last_done.load(Ordering::SeqCst)
    }

    fn mark_dead(&self, reason: String) {
        if !self.dead.swap(true, Ordering::SeqCst) {
            error!(
                "EC: in-process backend stopped ({}); using framework_tool until the service restarts",
                reason
            );
            if let Ok(mut r) = self.dead_reason.lock() {
                *r = Some(reason);
            }
        }
    }

    fn dead_reason(&self) -> Option<String> {
        if !self.is_dead() {
            return None;
        }
        self.dead_reason
            .lock()
            .ok()
            .and_then(|r| r.clone())
            .or_else(|| Some("stopped".into()))
    }
}

type Opener<C> = Arc<dyn Fn() -> Result<C, String> + Send + Sync>;

/// Runs closures against a context `C` on one dedicated thread. Generic so the failure handling
/// can be tested without hardware; the service uses it with [`Ctx`].
pub struct Engine<C: 'static> {
    label: String,
    opener: Opener<C>,
    cfg: EngineConfig,
    slot: tokio::sync::Mutex<Slot<C>>,
    shared: Arc<Shared>,
}

impl<C: Probe + 'static> Engine<C> {
    pub fn new(label: impl Into<String>, opener: Opener<C>, cfg: EngineConfig, disabled: Option<String>) -> Self {
        Self {
            label: label.into(),
            opener,
            cfg,
            slot: tokio::sync::Mutex::new(match disabled {
                Some(reason) => Slot::Disabled(reason),
                None => Slot::Idle,
            }),
            shared: Arc::new(Shared::default()),
        }
    }

    /// Run `f` on the worker thread. Any error means the caller should fall back to the CLI.
    pub async fn call<R, F>(&self, op: &'static str, f: F) -> Result<R, LibError>
    where
        R: Send + 'static,
        F: FnOnce(&mut C) -> Result<R, String> + Send + 'static,
    {
        if self.shared.is_dead() {
            return Err(LibError::Unavailable);
        }
        if self.shared.is_busy() {
            return Err(LibError::Busy);
        }
        let Some(tx) = self.sender().await else {
            return Err(LibError::Unavailable);
        };

        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let abandoned = Arc::new(AtomicBool::new(false));
        let (reply_tx, reply_rx) = oneshot::channel();
        let job = {
            let abandoned = abandoned.clone();
            let shared = self.shared.clone();
            let max_panics = self.cfg.max_panics;
            Job {
                id,
                run: Box::new(move |ctx: &mut C| {
                    // The caller already gave up and used the CLI; don't apply this late.
                    if abandoned.load(Ordering::SeqCst) {
                        return;
                    }
                    let out = run_guarded(op, ctx, f, &shared, max_panics);
                    let _ = reply_tx.send(out);
                }),
            }
        };

        if tx.send(job).is_err() {
            // Worker thread is gone. If it stopped on purpose `dead` is set; otherwise start over.
            if !self.shared.is_dead() {
                *self.slot.lock().await = Slot::Idle;
            }
            return Err(LibError::Unavailable);
        }

        match tokio::time::timeout(self.cfg.call_timeout, reply_rx).await {
            Ok(Ok(out)) => out,
            // The worker dropped the job without answering: it stopped.
            Ok(Err(_)) => Err(LibError::Unavailable),
            Err(_) => {
                abandoned.store(true, Ordering::SeqCst);
                self.shared.stuck.fetch_max(id, Ordering::SeqCst);
                warn!(
                    "EC: {} took longer than {:?}; using framework_tool until it returns",
                    op, self.cfg.call_timeout
                );
                Err(LibError::Busy)
            }
        }
    }

    /// Human-readable state, for logs and the versions endpoint.
    pub async fn status(&self) -> String {
        if let Some(reason) = self.shared.dead_reason() {
            return format!("stopped: {reason}");
        }
        if self.shared.is_busy() {
            return "busy".into();
        }
        match &*self.slot.lock().await {
            Slot::Ready(_) => "active".into(),
            Slot::Idle => "not started".into(),
            Slot::Waiting { reason, .. } => format!("unavailable: {reason}"),
            Slot::Disabled(reason) => format!("disabled: {reason}"),
        }
    }

    async fn sender(&self) -> Option<mpsc::Sender<Job<C>>> {
        let mut slot = self.slot.lock().await;
        let previous_backoff = match &*slot {
            Slot::Ready(tx) => return Some(tx.clone()),
            Slot::Disabled(_) => return None,
            Slot::Waiting { retry_at, backoff, .. } => {
                if Instant::now() < *retry_at {
                    return None;
                }
                Some(*backoff)
            }
            Slot::Idle => None,
        };
        if self.shared.is_dead() {
            return None;
        }

        match self.start_worker().await {
            Ok(tx) => {
                info!("EC: using {} in-process", self.label);
                *slot = Slot::Ready(tx.clone());
                Some(tx)
            }
            Err(reason) => {
                if self.shared.is_dead() {
                    return None;
                }
                let backoff = match previous_backoff {
                    Some(b) => (b * 2).min(self.cfg.retry_max),
                    None => {
                        info!("EC: in-process backend unavailable ({}); using framework_tool", reason);
                        self.cfg.retry_initial
                    }
                };
                debug!("EC: in-process backend unavailable ({}); next attempt in {:?}", reason, backoff);
                *slot = Slot::Waiting {
                    reason,
                    retry_at: Instant::now() + backoff,
                    backoff,
                };
                None
            }
        }
    }

    async fn start_worker(&self) -> Result<mpsc::Sender<Job<C>>, String> {
        let (tx, rx) = mpsc::channel::<Job<C>>();
        let (init_tx, init_rx) = oneshot::channel::<Result<(), String>>();
        let opener = self.opener.clone();
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("ec-worker".into())
            .spawn(move || match catch_unwind(AssertUnwindSafe(|| opener())) {
                Ok(Ok(ctx)) => {
                    let _ = init_tx.send(Ok(()));
                    worker_loop(ctx, rx, shared);
                }
                Ok(Err(e)) => {
                    let _ = init_tx.send(Err(e));
                }
                Err(payload) => {
                    let msg = panic_message(payload.as_ref());
                    shared.mark_dead(format!("opening the EC panicked: {msg}"));
                    let _ = init_tx.send(Err(msg));
                }
            })
            .map_err(|e| format!("failed to start EC worker thread: {e}"))?;

        match tokio::time::timeout(self.cfg.init_timeout, init_rx).await {
            Ok(Ok(Ok(()))) => Ok(tx),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err("EC worker exited while opening the EC".into()),
            Err(_) => Err(format!("opening the EC took longer than {:?}", self.cfg.init_timeout)),
        }
    }
}

fn worker_loop<C>(mut ctx: C, rx: mpsc::Receiver<Job<C>>, shared: Arc<Shared>) {
    while let Ok(job) = rx.recv() {
        (job.run)(&mut ctx);
        let was_busy = shared.is_busy();
        shared.last_done.fetch_max(job.id, Ordering::SeqCst);
        if was_busy && !shared.is_busy() {
            info!("EC: in-process backend responsive again");
        }
        if shared.is_dead() {
            break;
        }
    }
    // Dropping `rx` drops queued jobs; their callers see the channel close and use the CLI.
}

fn run_guarded<C: Probe, R, F>(op: &'static str, ctx: &mut C, f: F, shared: &Shared, max_panics: u32) -> Result<R, LibError>
where
    F: FnOnce(&mut C) -> Result<R, String>,
{
    match catch_unwind(AssertUnwindSafe(|| f(ctx))) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(LibError::Ec(e)),
        Err(payload) => {
            let msg = panic_message(payload.as_ref());
            let panics = shared.panics.fetch_add(1, Ordering::SeqCst) + 1;
            // A panic while the library held one of its global locks poisons it for good; the
            // probe goes through the same lock, so it fails (or panics) in that case.
            let healthy = catch_unwind(AssertUnwindSafe(|| ctx.probe())).unwrap_or(false);
            if !healthy {
                shared.mark_dead(format!("{op} panicked ({msg}) and the EC stopped responding"));
            } else if panics >= max_panics {
                shared.mark_dead(format!("{panics} panics in framework_lib, the last in {op}: {msg}"));
            } else {
                warn!(
                    "EC: {} panicked in framework_lib ({}); the EC still responds, keeping the in-process backend",
                    op, msg
                );
            }
            Err(LibError::Panicked(msg))
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// The service-wide engine

fn engine() -> &'static Engine<Ctx> {
    static ENGINE: OnceLock<Engine<Ctx>> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let disabled = match std::env::var("FRAMEWORK_CONTROL_EC_BACKEND") {
            Ok(v) if v.trim().eq_ignore_ascii_case("cli") => Some("FRAMEWORK_CONTROL_EC_BACKEND=cli".to_string()),
            _ => platform::unsupported_reason(),
        };
        Engine::new(label(), Arc::new(platform::open), EngineConfig::default(), disabled)
    })
}

/// Name of the in-process backend, e.g. "framework_lib 0.6.6 (Windows CrosEC driver)".
pub fn label() -> String {
    format!("framework_lib {} ({})", platform::LIB_VERSION, platform::DRIVER_LABEL)
}

/// Run an EC operation in-process. Errors mean "use the CLI".
pub async fn call<R, F>(op: &'static str, f: F) -> Result<R, LibError>
where
    R: Send + 'static,
    F: FnOnce(&mut Ctx) -> Result<R, String> + Send + 'static,
{
    engine().call(op, f).await
}

pub async fn status() -> String {
    engine().status().await
}

// ---------------------------------------------------------------------------------------------
// Mapping raw EC data to the service's types, shaped exactly like the parsed CLI output.

use super::framework_tool_parser::{FanReading, PowerBatteryInfo, ThermalParsed};
use std::collections::{BTreeMap, HashMap};

/// Memory-map layout (ChromeOS EC `ec_commands.h`, as used by framework_lib 0.6.6 `power.rs`).
pub mod memmap {
    pub const TEMP_SENSOR: u16 = 0x00;
    pub const TEMP_SENSOR_LEN: u16 = 0x0F;
    pub const FAN: u16 = 0x10;
    pub const FAN_LEN: u16 = 0x08;
    /// Battery block 0x40..0x60: volt, rate, cap (u32), flag/count/index (u8), dcap, dvlt, lfcc, ccnt (u32).
    pub const BATT: u16 = 0x40;
    pub const BATT_LEN: u16 = 0x20;

    pub const BATT_FLAG_AC_PRESENT: u8 = 0x01;
    pub const BATT_FLAG_BATT_PRESENT: u8 = 0x02;
    pub const BATT_FLAG_DISCHARGING: u8 = 0x04;
    pub const BATT_FLAG_CHARGING: u8 = 0x08;

    pub const FAN_SPEED_STALLED: u16 = 0xFFFE;
    pub const FAN_SPEED_NOT_PRESENT: u16 = 0xFFFF;
}

/// Temperature in °C, or None for the EC's "not present / error / not powered / not calibrated"
/// markers (which the CLI prints without a numeric reading, so the parser drops them).
pub fn temp_celsius(raw: u8) -> Option<i32> {
    match raw {
        0xFC..=0xFF => None,
        // EC reports Kelvin - 200; framework_lib prints `raw - 73` as °C.
        t => Some(t as i32 - 73),
    }
}

/// The key `parse_thermal` would extract from the CLI line `"  {name}: ..."`.
pub fn cli_key(name: &str) -> String {
    name.split_once(':').map(|(k, _)| k).unwrap_or(name).trim().to_string()
}

/// Build a [`ThermalParsed`] from memory-map reads.
/// `sensor_names`: EC sensor index -> name; missing names fall back to `Temp {i}` like framework_lib.
/// `fan_names`: per fan slot, as framework_lib names them for this platform.
pub fn thermal_from_raw(
    temps: &[u8],
    fans: &[u8],
    sensor_names: &HashMap<u8, String>,
    fan_names: &[String],
) -> ThermalParsed {
    let mut out_temps = BTreeMap::new();
    for (i, raw) in temps.iter().enumerate() {
        let Some(celsius) = temp_celsius(*raw) else { continue };
        let name = sensor_names
            .get(&(i as u8))
            .cloned()
            .unwrap_or_else(|| format!("Temp {}", i));
        out_temps.insert(cli_key(&name), celsius);
    }

    let mut out_fans = Vec::new();
    for (i, pair) in fans.chunks_exact(2).enumerate() {
        let rpm = match u16::from_le_bytes([pair[0], pair[1]]) {
            memmap::FAN_SPEED_NOT_PRESENT => continue,
            memmap::FAN_SPEED_STALLED => 0,
            rpm => rpm as u32,
        };
        let name = fan_names
            .get(i)
            .filter(|n| !n.trim().is_empty())
            .map(|n| cli_key(n))
            .unwrap_or_else(|| format!("Fan {}", i));
        out_fans.push(FanReading { name, rpm });
    }

    ThermalParsed {
        temps: out_temps,
        fans: out_fans,
    }
}

/// Charger state from `EC_CMD_CHARGE_STATE` (what `--power -vv` prints under "Charger Status").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerState {
    pub chg_voltage_mv: u32,
    pub chg_current_ma: u32,
    pub chg_input_current_ma: u32,
    pub batt_soc_pct: u32,
}

/// Battery fields decoded from the 0x40..0x60 memory-map block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryBlock {
    pub present_voltage_mv: u32,
    pub present_rate_ma: u32,
    pub remaining_capacity_mah: u32,
    pub flags: u8,
    pub design_capacity_mah: u32,
    pub design_voltage_mv: u32,
    pub lfcc_mah: u32,
    pub cycle_count: u32,
}

impl BatteryBlock {
    pub fn decode(block: &[u8]) -> Result<Self, String> {
        if block.len() < memmap::BATT_LEN as usize {
            return Err(format!("battery block too short ({} bytes)", block.len()));
        }
        let u32_at = |off: usize| u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        Ok(Self {
            present_voltage_mv: u32_at(0x00),
            present_rate_ma: u32_at(0x04),
            remaining_capacity_mah: u32_at(0x08),
            flags: block[0x0C],
            design_capacity_mah: u32_at(0x10),
            design_voltage_mv: u32_at(0x14),
            lfcc_mah: u32_at(0x18),
            cycle_count: u32_at(0x1C),
        })
    }

    pub fn battery_present(&self) -> bool {
        self.flags & memmap::BATT_FLAG_BATT_PRESENT != 0
    }
}

/// Build a [`PowerBatteryInfo`] like `parse_power(framework_tool --power -vv)`.
/// Like the CLI (which exits non-zero), it's an error when no battery is connected.
pub fn power_from_raw(batt: &BatteryBlock, charger: Option<&ChargerState>) -> Result<PowerBatteryInfo, String> {
    if !batt.battery_present() {
        return Err("no battery connected".into());
    }
    let percentage = if batt.lfcc_mah > 0 {
        Some((100u64 * batt.remaining_capacity_mah as u64 / batt.lfcc_mah as u64) as u32)
    } else {
        None
    };
    Ok(PowerBatteryInfo {
        ac_present: Some(batt.flags & memmap::BATT_FLAG_AC_PRESENT != 0),
        battery_present: Some(true),
        last_full_charge_capacity_mah: Some(batt.lfcc_mah),
        remaining_capacity_mah: Some(batt.remaining_capacity_mah),
        percentage,
        soc_pct: charger.map(|c| c.batt_soc_pct),
        present_voltage_mv: Some(batt.present_voltage_mv),
        present_rate_ma: Some(batt.present_rate_ma),
        charger_voltage_mv: charger.map(|c| c.chg_voltage_mv),
        charger_current_ma: charger.map(|c| c.chg_current_ma),
        charge_input_current_ma: charger.map(|c| c.chg_input_current_ma),
        design_capacity_mah: Some(batt.design_capacity_mah),
        design_voltage_mv: Some(batt.design_voltage_mv),
        cycle_count: Some(batt.cycle_count),
        // The CLI only prints these lines when the flag is set, so the parser yields Some(true) or None.
        charging: (batt.flags & memmap::BATT_FLAG_CHARGING != 0).then_some(true),
        discharging: (batt.flags & memmap::BATT_FLAG_DISCHARGING != 0).then_some(true),
    })
}

/// Charge current for `--charge-rate-limit <rate>`: the CLI parses the rate we format with 3
/// decimals and multiplies it by the design capacity.
pub fn charge_rate_current_ma(rate_c: f32, design_capacity_mah: u32) -> u32 {
    let rate: f32 = format!("{:.3}", rate_c).parse().unwrap_or(rate_c);
    (rate * design_capacity_mah as f32) as u32
}

// ---------------------------------------------------------------------------------------------
// Platform backend

#[cfg(any(windows, target_os = "linux"))]
mod platform {
    use std::collections::HashMap;

    use framework_lib::chromium_ec::commands::{ChargeStateCmd, EcRequestChargeStateGetV0};
    use framework_lib::chromium_ec::{CrosEc, CrosEcDriver, CrosEcDriverType, EcCurrentImage, EcError, EcRequestRaw};

    use super::super::framework_tool_parser::{BatteryChargeLimitInfo, PowerBatteryInfo, ThermalParsed, VersionsParsed};
    use super::{memmap, BatteryBlock, ChargerState, Probe};

    pub const LIB_VERSION: &str = framework_lib::built_info::PKG_VERSION;

    #[cfg(windows)]
    const DRIVER: CrosEcDriverType = CrosEcDriverType::Windows;
    #[cfg(windows)]
    pub const DRIVER_LABEL: &str = "Windows CrosEC driver";

    #[cfg(target_os = "linux")]
    const DRIVER: CrosEcDriverType = CrosEcDriverType::CrosEc;
    #[cfg(target_os = "linux")]
    pub const DRIVER_LABEL: &str = "/dev/cros_ec";

    pub fn unsupported_reason() -> Option<String> {
        None
    }

    fn ec_err(e: EcError) -> String {
        format!("{:?}", e)
    }

    /// Open the EC on the worker thread. Never selects the port-I/O driver.
    pub fn open() -> Result<Ctx, String> {
        let Some(ec) = CrosEc::with(DRIVER) else {
            #[cfg(target_os = "linux")]
            return Err("no /dev/cros_ec (cros_ec kernel driver not loaded)".into());
            #[cfg(windows)]
            return Err("Windows CrosEC driver not available".into());
        };
        #[cfg(target_os = "linux")]
        {
            // framework_lib 0.6.6 panics while holding its global fd lock if /dev/cros_ec exists
            // but can't be opened, which would poison it for the life of the process.
            std::fs::File::open("/dev/cros_ec").map_err(|e| format!("cannot open /dev/cros_ec: {e}"))?;
        }
        ec.check_mem_magic().map_err(ec_err)?;
        Ok(Ctx {
            ec,
            sensor_names: HashMap::new(),
            fan_names: None,
        })
    }

    pub struct Ctx {
        ec: CrosEc,
        /// Sensor names never change for a given EC; only successful lookups are cached.
        sensor_names: HashMap<u8, String>,
        /// Per fan slot, as framework_lib names them for this platform.
        fan_names: Option<Vec<String>>,
    }

    impl Probe for Ctx {
        fn probe(&mut self) -> bool {
            self.ec.check_mem_magic().is_ok()
        }
    }

    impl Ctx {
        fn read_mem(&self, offset: u16, len: u16) -> Result<Vec<u8>, String> {
            let data = self
                .ec
                .read_memory(offset, len)
                .ok_or_else(|| format!("EC memory read failed (offset {:#04x}, {} bytes)", offset, len))?;
            if data.len() != len as usize {
                return Err(format!(
                    "EC memory read returned {} bytes instead of {} (offset {:#04x})",
                    data.len(),
                    len,
                    offset
                ));
            }
            Ok(data)
        }

        fn battery_block(&self) -> Result<BatteryBlock, String> {
            BatteryBlock::decode(&self.read_mem(memmap::BATT, memmap::BATT_LEN)?)
        }

        /// Equivalent of `framework_tool --thermal`, without the per-call sensor-name lookups.
        pub fn thermal(&mut self) -> Result<ThermalParsed, String> {
            if self.fan_names.is_none() {
                // Once: let framework_lib name the fan slots for this platform.
                let full = framework_lib::power::get_thermal(&self.ec).map_err(ec_err)?;
                let mut names = vec![String::new(); (memmap::FAN_LEN / 2) as usize];
                for fan in &full.fans {
                    if let Some(slot) = names.get_mut(fan.index) {
                        *slot = fan.name.clone();
                    }
                }
                self.fan_names = Some(names);
            }

            let temps = self.read_mem(memmap::TEMP_SENSOR, memmap::TEMP_SENSOR_LEN)?;
            let fans = self.read_mem(memmap::FAN, memmap::FAN_LEN)?;
            for (i, raw) in temps.iter().enumerate() {
                let i = i as u8;
                if super::temp_celsius(*raw).is_some() && !self.sensor_names.contains_key(&i) {
                    if let Ok(name) = self.ec.get_temp_sensor_name(i) {
                        self.sensor_names.insert(i, name);
                    }
                }
            }
            Ok(super::thermal_from_raw(
                &temps,
                &fans,
                &self.sensor_names,
                self.fan_names.as_deref().unwrap_or(&[]),
            ))
        }

        /// Equivalent of `framework_tool --power -vv`.
        pub fn power(&mut self) -> Result<PowerBatteryInfo, String> {
            let batt = self.battery_block()?;
            if !batt.battery_present() {
                return Err("no battery connected".into());
            }
            let charger = EcRequestChargeStateGetV0 {
                cmd: ChargeStateCmd::GetState as u8,
                param: 0,
            }
            .send_command(&self.ec)
            .ok()
            .map(|r| ChargerState {
                chg_voltage_mv: { r.chg_voltage },
                chg_current_ma: { r.chg_current },
                chg_input_current_ma: { r.chg_input_current },
                batt_soc_pct: { r.batt_state_of_charge },
            });
            super::power_from_raw(&batt, charger.as_ref())
        }

        /// Equivalent of `framework_tool --charge-limit`.
        pub fn charge_limit_get(&mut self) -> Result<BatteryChargeLimitInfo, String> {
            let (min, max) = self.ec.get_charge_limit().map_err(ec_err)?;
            Ok(BatteryChargeLimitInfo {
                charge_limit_min_pct: Some(min),
                charge_limit_max_pct: Some(max),
            })
        }

        /// Equivalent of `framework_tool --charge-limit <max>`: keeps the current minimum.
        pub fn charge_limit_set(&mut self, max_pct: u8) -> Result<(), String> {
            if max_pct < 25 {
                return Err("Not recommended to set charge limit below 25%".into());
            }
            if max_pct > 100 {
                return Err("Charge limit cannot be set above 100%".into());
            }
            let (cur_min, _) = self.ec.get_charge_limit().map_err(ec_err)?;
            self.ec.set_charge_limit(cur_min, max_pct).map_err(ec_err)
        }

        /// Equivalent of `framework_tool --charge-rate-limit <rate> [soc]` without its stdout
        /// output and without depending on a non-zero full-charge capacity.
        pub fn charge_rate_limit_set(&mut self, rate_c: f32, soc_threshold_pct: Option<u8>) -> Result<(), String> {
            let batt = self.battery_block()?;
            if !batt.battery_present() {
                return Err("No battery present".into());
            }
            let current = super::charge_rate_current_ma(rate_c, batt.design_capacity_mah);
            self.ec
                .set_charge_current_limit(current, soc_threshold_pct.map(u32::from))
                .map_err(ec_err)
        }

        /// Equivalent of `framework_tool --fansetduty [fan] <percent>`.
        pub fn set_fan_duty(&mut self, percent: u32, fan_index: Option<u32>) -> Result<(), String> {
            self.ec.fan_set_duty(fan_index, percent).map_err(ec_err)
        }

        /// Equivalent of `framework_tool --autofanctrl`.
        pub fn autofanctrl(&mut self) -> Result<(), String> {
            self.ec.autofanctrl(None).map_err(ec_err)
        }

        /// The fields `parse_versions` reads from `framework_tool --versions`. `tool_version` is
        /// left for the caller (it describes the CLI binary).
        pub fn versions(&mut self) -> Result<VersionsParsed, String> {
            use framework_lib::smbios;
            let unknown = || "Unknown".to_string();
            let mainboard_type = smbios::get_product_name()
                .map(|s| s.trim().to_string())
                .unwrap_or_else(unknown);
            let mainboard_revision = smbios::get_baseboard_version()
                .map(|v| format!("{:?}", v))
                .unwrap_or_else(unknown);
            let bios = smbios::get_smbios().and_then(|s| {
                s.structures().find_map(|r| match r {
                    Ok(dmidecode::Structure::Bios(b)) => Some((
                        b.bios_version.trim().to_string(),
                        b.bios_release_date.trim().to_string(),
                    )),
                    _ => None,
                })
            });
            let (uefi_version, uefi_release_date) = match bios {
                Some((v, d)) => (Some(v), Some(d)),
                None => (Some(unknown()), None),
            };
            let ec_build_version = self.ec.version_info().unwrap_or_else(|_| "UNKNOWN".to_string());
            let ec_current_image = match self.ec.flash_version() {
                Some((_, _, EcCurrentImage::RO)) => "RO",
                Some((_, _, EcCurrentImage::RW)) => "RW",
                _ => "Unknown",
            };
            Ok(VersionsParsed {
                tool_version: None,
                mainboard_type: Some(mainboard_type),
                mainboard_revision: Some(mainboard_revision),
                uefi_version,
                uefi_release_date,
                ec_build_version: Some(ec_build_version.trim().to_string()),
                ec_current_image: Some(ec_current_image.to_string()),
                ec_backend: None,
            })
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::super::framework_tool_parser::{BatteryChargeLimitInfo, PowerBatteryInfo, ThermalParsed, VersionsParsed};
    use super::Probe;

    pub const LIB_VERSION: &str = "n/a";
    pub const DRIVER_LABEL: &str = "unsupported platform";

    pub fn unsupported_reason() -> Option<String> {
        Some("unsupported platform".into())
    }

    pub fn open() -> Result<Ctx, String> {
        Err("unsupported platform".into())
    }

    /// Never constructed on this platform.
    pub struct Ctx;

    impl Probe for Ctx {
        fn probe(&mut self) -> bool {
            false
        }
    }

    const UNSUPPORTED: &str = "unsupported platform";

    impl Ctx {
        pub fn thermal(&mut self) -> Result<ThermalParsed, String> {
            Err(UNSUPPORTED.into())
        }
        pub fn power(&mut self) -> Result<PowerBatteryInfo, String> {
            Err(UNSUPPORTED.into())
        }
        pub fn charge_limit_get(&mut self) -> Result<BatteryChargeLimitInfo, String> {
            Err(UNSUPPORTED.into())
        }
        pub fn charge_limit_set(&mut self, _max_pct: u8) -> Result<(), String> {
            Err(UNSUPPORTED.into())
        }
        pub fn charge_rate_limit_set(&mut self, _rate_c: f32, _soc: Option<u8>) -> Result<(), String> {
            Err(UNSUPPORTED.into())
        }
        pub fn set_fan_duty(&mut self, _percent: u32, _fan_index: Option<u32>) -> Result<(), String> {
            Err(UNSUPPORTED.into())
        }
        pub fn autofanctrl(&mut self) -> Result<(), String> {
            Err(UNSUPPORTED.into())
        }
        pub fn versions(&mut self) -> Result<VersionsParsed, String> {
            Err(UNSUPPORTED.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::framework_tool_parser::{parse_power, parse_thermal};
    use std::sync::atomic::AtomicUsize;

    // ---- engine failure handling -------------------------------------------------------------

    #[derive(Default)]
    struct FakeState {
        healthy: AtomicBool,
        opened: AtomicUsize,
        executed: AtomicUsize,
    }

    struct Fake(Arc<FakeState>);

    impl Probe for Fake {
        fn probe(&mut self) -> bool {
            self.0.healthy.load(Ordering::SeqCst)
        }
    }

    fn test_cfg() -> EngineConfig {
        EngineConfig {
            call_timeout: Duration::from_millis(200),
            init_timeout: Duration::from_secs(2),
            retry_initial: Duration::from_millis(100),
            retry_max: Duration::from_millis(400),
            max_panics: 3,
        }
    }

    fn fake_engine(state: Arc<FakeState>) -> Engine<Fake> {
        state.healthy.store(true, Ordering::SeqCst);
        let s = state.clone();
        Engine::new(
            "fake",
            Arc::new(move || -> Result<Fake, String> {
                s.opened.fetch_add(1, Ordering::SeqCst);
                Ok(Fake(s.clone()))
            }),
            test_cfg(),
            None,
        )
    }

    #[tokio::test]
    async fn ok_and_ec_errors_keep_backend() {
        let st = Arc::new(FakeState::default());
        let e = fake_engine(st.clone());
        assert_eq!(e.call("op", |_c| Ok(7)).await, Ok(7));
        assert_eq!(
            e.call("op", |_c| Err::<u8, _>("bad".to_string())).await,
            Err(LibError::Ec("bad".into()))
        );
        assert_eq!(e.call("op", |_c| Ok(8)).await, Ok(8));
        assert_eq!(st.opened.load(Ordering::SeqCst), 1);
        assert_eq!(e.status().await, "active");
    }

    #[tokio::test]
    async fn panic_with_healthy_probe_keeps_backend() {
        let st = Arc::new(FakeState::default());
        let e = fake_engine(st.clone());
        let r = e.call("op", |_c| -> Result<u8, String> { panic!("boom") }).await;
        assert_eq!(r, Err(LibError::Panicked("boom".into())));
        assert_eq!(e.call("op", |_c| Ok(1)).await, Ok(1));
        assert_eq!(e.status().await, "active");
    }

    #[tokio::test]
    async fn panic_with_failed_probe_stops_backend() {
        let st = Arc::new(FakeState::default());
        let e = fake_engine(st.clone());
        let r = e
            .call("op", |c: &mut Fake| -> Result<u8, String> {
                c.0.healthy.store(false, Ordering::SeqCst);
                panic!("poisoned")
            })
            .await;
        assert!(matches!(r, Err(LibError::Panicked(_))));
        assert_eq!(e.call("op", |_c| Ok(1)).await, Err(LibError::Unavailable));
        assert!(e.status().await.starts_with("stopped:"));
        // Never re-opened after stopping.
        assert_eq!(st.opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn repeated_panics_stop_backend() {
        let st = Arc::new(FakeState::default());
        let e = fake_engine(st.clone());
        for _ in 0..3 {
            let _ = e.call("op", |_c| -> Result<u8, String> { panic!("again") }).await;
        }
        assert_eq!(e.call("op", |_c| Ok(1)).await, Err(LibError::Unavailable));
    }

    #[tokio::test]
    async fn timeout_marks_busy_then_recovers_and_drops_queued_jobs() {
        let st = Arc::new(FakeState::default());
        let e = Arc::new(fake_engine(st.clone()));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        // A call that blocks the worker past the deadline.
        let r = e
            .call("slow", move |_c| {
                let _ = release_rx.recv();
                Ok(1)
            })
            .await;
        assert_eq!(r, Err(LibError::Busy));
        // While busy, calls are refused immediately (the caller uses the CLI).
        let s = st.clone();
        assert_eq!(
            e.call("write", move |_c| {
                s.executed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await,
            Err(LibError::Busy)
        );
        assert_eq!(e.status().await, "busy");

        release_tx.send(()).unwrap();
        // Wait for the worker to finish the slow job.
        for _ in 0..50 {
            if !e.shared.is_busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(e.call("op", |_c| Ok(2)).await, Ok(2));
        // The refused write never ran.
        assert_eq!(st.executed.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn abandoned_job_queued_behind_a_hang_is_not_executed() {
        let st = Arc::new(FakeState::default());
        let e = Arc::new(fake_engine(st.clone()));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        // Both calls are queued before either times out.
        let e1 = e.clone();
        let slow = tokio::spawn(async move {
            e1.call("slow", move |_c| {
                let _ = release_rx.recv();
                Ok(())
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let s = st.clone();
        let e2 = e.clone();
        let queued = tokio::spawn(async move {
            e2.call("write", move |_c| {
                s.executed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
        });
        assert_eq!(slow.await.unwrap(), Err(LibError::Busy));
        assert_eq!(queued.await.unwrap(), Err(LibError::Busy));

        release_tx.send(()).unwrap();
        for _ in 0..50 {
            if !e.shared.is_busy() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(e.call("op", |_c| Ok(3)).await, Ok(3));
        assert_eq!(st.executed.load(Ordering::SeqCst), 0, "late write must be skipped");
    }

    #[tokio::test]
    async fn open_failure_retries_with_backoff() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a = attempts.clone();
        let e: Engine<Fake> = Engine::new(
            "fake",
            Arc::new(move || -> Result<Fake, String> {
                let n = a.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err("no driver".to_string())
                } else {
                    let st = Arc::new(FakeState::default());
                    st.healthy.store(true, Ordering::SeqCst);
                    Ok(Fake(st))
                }
            }),
            test_cfg(),
            None,
        );
        assert_eq!(e.call("op", |_c| Ok(1)).await, Err(LibError::Unavailable));
        assert!(e.status().await.contains("no driver"));
        // Within the backoff window no new attempt is made.
        assert_eq!(e.call("op", |_c| Ok(1)).await, Err(LibError::Unavailable));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(e.call("op", |_c| Ok(1)).await, Ok(1));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn open_panic_stops_backend() {
        let e: Engine<Fake> = Engine::new(
            "fake",
            Arc::new(|| -> Result<Fake, String> { panic!("init boom") }),
            test_cfg(),
            None,
        );
        assert_eq!(e.call("op", |_c| Ok(1)).await, Err(LibError::Unavailable));
        assert!(e.status().await.starts_with("stopped:"));
    }

    #[tokio::test]
    async fn disabled_engine_never_opens() {
        let opened = Arc::new(AtomicUsize::new(0));
        let o = opened.clone();
        let e: Engine<Fake> = Engine::new(
            "fake",
            Arc::new(move || -> Result<Fake, String> {
                o.fetch_add(1, Ordering::SeqCst);
                Err("x".to_string())
            }),
            test_cfg(),
            Some("FRAMEWORK_CONTROL_EC_BACKEND=cli".into()),
        );
        assert_eq!(e.call("op", |_c| Ok(1)).await, Err(LibError::Unavailable));
        assert_eq!(opened.load(Ordering::SeqCst), 0);
        assert!(e.status().await.starts_with("disabled:"));
    }

    // ---- parity with the parsed CLI output ---------------------------------------------------

    /// Render `framework_tool --thermal` (0.6.6 `print_thermal`) for the same raw data.
    fn render_cli_thermal(temps: &[u8], fans: &[u8], names: &HashMap<u8, String>, fan_names: &[String]) -> String {
        let mut out = String::new();
        let width = names.values().map(|n| n.len() + 1).max().unwrap_or(13);
        for (i, raw) in temps.iter().enumerate() {
            if *raw == 0xFF {
                continue; // NotPresent sensors are skipped
            }
            let name = format!("{}:", names.get(&(i as u8)).cloned().unwrap_or(format!("Temp {}", i)));
            let reading = match *raw {
                0xFE => "Error".to_string(),
                0xFD => "NotPowered".to_string(),
                0xFC => "NotCalibrated".to_string(),
                t => format!("{} C", t - 73),
            };
            out += &format!("  {:<width$} {:>4}\n", name, reading, width = width);
        }
        for (i, pair) in fans.chunks_exact(2).enumerate() {
            let raw = u16::from_le_bytes([pair[0], pair[1]]);
            let name = format!("{}:", fan_names[i]);
            match raw {
                0xFFFF => {} // "Not present" goes to the log, not stdout
                0xFFFE => out += &format!("  {:<width$} {:>4} RPM (Stalled)\n", name, 0xFFFEu16, width = width),
                rpm => out += &format!("  {:<width$} {:>4} RPM\n", name, rpm, width = width),
            }
        }
        out += "  AP Throttle Status\n    Soft (AMD SPPT):  false\n    Hard (PROCHOT#):  false\n";
        out
    }

    fn fw16_names() -> (HashMap<u8, String>, Vec<String>) {
        let names: HashMap<u8, String> = [
            (0u8, "ambient_f75303@4d"),
            (1, "charger_f75303@4d"),
            (2, "apu_f75303@4d"),
            (3, "cpu@4c"),
            (4, "gpu_vr_f75303@4d"),
            (5, "gpu_vram_f75303@4d"),
            (6, "gpu_amb_f75303@4d"),
            (7, "gpu_temp"),
        ]
        .into_iter()
        .map(|(i, n)| (i, n.to_string()))
        .collect();
        let fans = ["Left Fan", "Right Fan", "Fan 2", "Fan 3"].iter().map(|s| s.to_string()).collect();
        (names, fans)
    }

    #[test]
    fn thermal_matches_parsed_cli_output() {
        let (names, fan_names) = fw16_names();
        // 40,38,61,55 °C, three dGPU sensors at 0 °C, dGPU temp NotPowered, rest not present.
        let mut temps = vec![113u8, 111, 134, 128, 73, 73, 73, 0xFD];
        temps.resize(15, 0xFF);
        let mut fans = Vec::new();
        for rpm in [2165u16, 0xFFFE, 0xFFFF, 0xFFFF] {
            fans.extend_from_slice(&rpm.to_le_bytes());
        }
        let ours = thermal_from_raw(&temps, &fans, &names, &fan_names);
        let cli = parse_thermal(&render_cli_thermal(&temps, &fans, &names, &fan_names));
        assert_eq!(ours.temps, cli.temps);
        let ours_fans: Vec<_> = ours.fans.iter().map(|f| (f.name.clone(), f.rpm)).collect();
        let cli_fans: Vec<_> = cli.fans.iter().map(|f| (f.name.clone(), f.rpm)).collect();
        assert_eq!(ours_fans, cli_fans);
        assert_eq!(ours.temps.get("apu_f75303@4d"), Some(&61));
        assert!(!ours.temps.contains_key("gpu_temp"));
        assert_eq!(ours_fans, vec![("Left Fan".to_string(), 2165), ("Right Fan".to_string(), 0)]);
    }

    #[test]
    fn thermal_falls_back_to_index_names_like_framework_lib() {
        let temps = [100u8, 0xFF, 120];
        let fans = 1000u16.to_le_bytes();
        let t = thermal_from_raw(&temps, &fans, &HashMap::new(), &[]);
        assert_eq!(t.temps.get("Temp 0"), Some(&27));
        assert_eq!(t.temps.get("Temp 2"), Some(&47));
        assert_eq!(t.fans[0].name, "Fan 0");
    }

    fn battery_bytes(volt: u32, rate: u32, cap: u32, flags: u8, dcap: u32, dvlt: u32, lfcc: u32, ccnt: u32) -> Vec<u8> {
        let mut b = vec![0u8; 0x20];
        b[0x00..0x04].copy_from_slice(&volt.to_le_bytes());
        b[0x04..0x08].copy_from_slice(&rate.to_le_bytes());
        b[0x08..0x0C].copy_from_slice(&cap.to_le_bytes());
        b[0x0C] = flags;
        b[0x10..0x14].copy_from_slice(&dcap.to_le_bytes());
        b[0x14..0x18].copy_from_slice(&dvlt.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&lfcc.to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&ccnt.to_le_bytes());
        b
    }

    /// Render `framework_tool --power -vv` (0.6.6 `get_charge_state` + `print_battery_information`).
    fn render_cli_power(b: &BatteryBlock, c: &ChargerState) -> String {
        let ac = if b.flags & 1 != 0 { "connected" } else { "not connected" };
        let mut s = String::new();
        s += "Battery Cutoff:     Not cut off\n";
        s += "Charger Status\n";
        s += &format!("  AC is:            {}\n", ac);
        s += &format!("  Charger Voltage:  {}mV\n", c.chg_voltage_mv);
        s += &format!("  Charger Current:  {}mA\n", c.chg_current_ma);
        s += "                    0.65C\n";
        s += &format!("  Chg Input Current:{}mA\n", c.chg_input_current_ma);
        s += &format!("  Battery SoC:      {}%\n", c.batt_soc_pct);
        s += "Battery Status\n";
        s += &format!("  AC is:            {}\n", ac);
        s += "  Battery is:       connected\n";
        s += &format!("  Battery LFCC:     {:#?} mAh (Last Full Charge Capacity)\n", b.lfcc_mah);
        s += &format!("  Battery Capacity: {} mAh\n", b.remaining_capacity_mah);
        s += "                    40.123 Wh\n";
        s += &format!("  Charge level:     {:?}%\n", 100 * b.remaining_capacity_mah / b.lfcc_mah);
        s += "  Manufacturer:     NVT\n  Model Number:     FRANDBA\n  Serial Number:    0204\n  Battery Type:     LION\n";
        s += &format!(
            "  Present Voltage:  {}.{} V\n",
            b.present_voltage_mv / 1000,
            b.present_voltage_mv % 1000
        );
        s += &format!("  Present Rate:     {} mA\n", b.present_rate_ma);
        s += &format!("  Design Capacity:  {} mAh\n", b.design_capacity_mah);
        s += "                    85.0 Wh\n";
        s += &format!(
            "  Design Voltage:   {}.{} V\n",
            b.design_voltage_mv / 1000,
            b.design_voltage_mv % 1000
        );
        s += &format!("  Cycle Count:      {}\n", b.cycle_count);
        if b.flags & memmap::BATT_FLAG_DISCHARGING != 0 {
            s += "  Battery discharging\n";
        }
        if b.flags & memmap::BATT_FLAG_CHARGING != 0 {
            s += "  Battery charging\n";
        }
        s
    }

    #[test]
    fn power_matches_parsed_cli_output() {
        // Voltages with a 3-digit mV remainder: the CLI prints `{}.{}` without zero padding.
        let charger = ChargerState {
            chg_voltage_mv: 17800,
            chg_current_ma: 3568,
            chg_input_current_ma: 4400,
            batt_soc_pct: 52,
        };
        for flags in [0x0Bu8, 0x06, 0x03] {
            let b = BatteryBlock::decode(&battery_bytes(16591, 3221, 2685, flags, 5491, 15480, 5182, 58)).unwrap();
            let ours = power_from_raw(&b, Some(&charger)).unwrap();
            let cli = parse_power(&render_cli_power(&b, &charger));
            assert_eq!(format!("{:?}", ours), format!("{:?}", cli), "flags {flags:#x}");
        }
    }

    #[test]
    fn power_is_exact_where_cli_text_loses_precision() {
        // 16051 mV prints as "16.51 V" in framework_tool 0.6.6, which parses back as 16510.
        let b = BatteryBlock::decode(&battery_bytes(16051, 0, 100, 0x03, 5491, 15048, 5000, 1)).unwrap();
        let ours = power_from_raw(&b, None).unwrap();
        assert_eq!(ours.present_voltage_mv, Some(16051));
        assert_eq!(ours.design_voltage_mv, Some(15048));
        assert_eq!(ours.soc_pct, None);
    }

    #[test]
    fn power_without_battery_is_an_error_like_the_cli() {
        let b = BatteryBlock::decode(&battery_bytes(0, 0, 0, 0x01, 0, 0, 0, 0)).unwrap();
        assert!(power_from_raw(&b, None).is_err());
    }

    #[test]
    fn power_survives_zero_full_charge_capacity() {
        // framework_lib's own power_info divides by LFCC and panics here.
        let b = BatteryBlock::decode(&battery_bytes(16000, 0, 100, 0x03, 5491, 15480, 0, 1)).unwrap();
        let p = power_from_raw(&b, None).unwrap();
        assert_eq!(p.percentage, None);
        assert_eq!(p.remaining_capacity_mah, Some(100));
    }

    #[test]
    fn charge_rate_current_matches_cli_rounding() {
        assert_eq!(charge_rate_current_ma(0.65, 5491), (0.65f32 * 5491.0) as u32);
        assert_eq!(charge_rate_current_ma(0.6499999, 5491), (0.65f32 * 5491.0) as u32);
    }
}
