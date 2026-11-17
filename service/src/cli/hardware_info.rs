//! Read-only hardware status views: USB-C PD ports, expansion bay, input deck, privacy switches
//! and chassis intrusion.
//!
//! Like the rest of the EC access, each view is read in-process through `framework_lib` first (see
//! [`ec_lib`](super::ec_lib)) and falls back to parsing `framework_tool` output. Both paths return
//! the same values, so this module holds the types, the decoding of the raw EC responses and the
//! CLI parsers side by side, with tests that compare them.
//!
//! The in-process side decodes the raw host command responses here instead of calling the library's
//! helpers: those print to stdout or panic on values they don't know, and a panic counts against
//! the EC worker that fan control shares.
//!
//! A view is `None` when this machine doesn't have it (the EC rejects the command, or the CLI
//! prints nothing recognisable). That is the normal answer on most models, not an error.

use std::collections::HashMap;
use std::time::Duration;

use poem_openapi::Object;
use tracing::debug;

use super::framework_tool::FrameworkTool;
use crate::utils::global_cache;

/// All Framework systems so far have at most 4 PD ports; the EC rejects indexes it doesn't have.
pub const PD_PORT_COUNT: u8 = 4;

/// One USB-C port, as `framework_tool --pdports` reports it. Fields the CLI only prints for a
/// connected port (or one with a PD contract) are `None` otherwise.
#[derive(Debug, Clone, PartialEq, Default, Object)]
pub struct PdPortInfo {
    /// EC port index (0-3)
    pub port: u8,
    /// Something is attached to the port
    pub connected: bool,
    /// A USB Power Delivery contract is in place
    pub pd_contract: bool,
    /// "Sink" (the port takes power), "Source" (it supplies power) or "Unknown"
    pub power_role: String,
    /// "Ufp", "Dfp", "Disconnected" or "Unknown"
    pub data_role: String,
    pub vconn: Option<bool>,
    /// Negotiated voltage
    pub voltage_mv: Option<u32>,
    /// Negotiated current
    pub current_ma: Option<u32>,
    /// "CC1", "CC2", "CC1 (Debug)", "CC2 (Debug)" or "Unknown"
    pub cc_polarity: Option<String>,
    /// What the port partner is: "Source", "Sink", "Debug", "Audio", "PoweredAccessory", ...
    pub partner: Option<String>,
    pub epr_active: Option<bool>,
    pub epr_supported: Option<bool>,
    /// For a sink port: whether this is the port the system is charging from
    pub sink_active: Option<bool>,
    /// DisplayPort alt mode status flags, empty when the port is not in DP alt mode
    pub dp_alt_mode: Vec<String>,
}

/// Framework 16 expansion bay, as `framework_tool --expansion-bay` reports it.
#[derive(Debug, Clone, PartialEq, Default, Object)]
pub struct ExpansionBayInfo {
    pub enabled: bool,
    pub fault: bool,
    pub door_closed: bool,
    /// "DualInterposer", "SingleInterposer", "UmaFans", "NoModule" or "BadConnection(a, b)"
    pub board: String,
    pub serial: Option<String>,
    /// "Pcie8x1", "Pcie4x1", "Pcie4x2" or "Unknown (n)"
    pub pcie_config: Option<String>,
    /// "NvidiaGn22", "GpuAmdR23M", "FanOnly", "SsdHolder", "PcieAccessory", "Initializing" or "Unknown (n)"
    pub vendor: Option<String>,
}

/// Framework 16 input deck, as `framework_tool --inputdeck` reports it.
#[derive(Debug, Clone, PartialEq, Default, Object)]
pub struct InputDeckInfo {
    /// "Off", "Disconnected", "TurningOn", "On", "ForceOff", "ForceOn" or "NoDetection"
    pub state: String,
    pub touchpad_present: bool,
    /// Module type per top-row position, left to right: "KeyboardA", "KeyboardB", "GenericA",
    /// "GenericB", "GenericC", "FullWidth", ..., "Disconnected"
    pub top_row: Vec<String>,
}

/// Privacy switches, as `framework_tool --privacy` reports them.
#[derive(Debug, Clone, PartialEq, Default, Object)]
pub struct PrivacyInfo {
    /// false when the switch has cut the microphone off
    pub microphone_connected: bool,
    /// true while the camera is powered. false when the switch has cut it off, and also while
    /// nothing uses the camera: seen on a Framework 16, where the OS powers the idle camera down
    /// and the EC then reports it like a cut-off one.
    pub camera_connected: bool,
}

/// Chassis intrusion status, as `framework_tool --intrusion` reports it.
#[derive(Debug, Clone, PartialEq, Default, Object)]
pub struct IntrusionInfo {
    pub currently_open: bool,
    pub ever_opened: bool,
    pub coin_cell_ever_removed: bool,
    pub total_opened: u32,
    pub opened_while_off: u32,
}

/// Everything `/api/hardware` returns. A section is absent when this machine doesn't have it.
#[derive(Debug, Clone, PartialEq, Default, Object)]
pub struct HardwareInfo {
    pub pd_ports: Option<Vec<PdPortInfo>>,
    pub expansion_bay: Option<ExpansionBayInfo>,
    pub input_deck: Option<InputDeckInfo>,
    pub privacy: Option<PrivacyInfo>,
    pub intrusion: Option<IntrusionInfo>,
}

// ---------------------------------------------------------------------------------------------
// Decoding raw EC responses (names match framework_lib 0.6.6's `{:?}` output, which the CLI prints)

/// `EC_CMD_GET_PD_PORT_STATE` response fields.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PdPortRaw {
    pub c_state: u8,
    pub pd_state: u8,
    pub power_role: u8,
    pub data_role: u8,
    pub vconn: u8,
    pub epr_active: u8,
    pub epr_support: u8,
    pub cc_polarity: u8,
    pub voltage_mv: u16,
    pub current_ma: u16,
    pub active_port: u8,
    pub alt_mode: u8,
}

const DP_ALT_MODE_FLAGS: [(u8, &str); 8] = [
    (0x01, "DFP_D Connected"),
    (0x02, "UFP_D Connected"),
    (0x04, "Power Low"),
    (0x08, "Enabled"),
    (0x10, "Multi-Function"),
    (0x20, "USB Config"),
    (0x40, "Exit Request"),
    (0x80, "HPD High"),
];

pub fn pd_port_from_raw(port: u8, r: &PdPortRaw) -> PdPortInfo {
    let connected = r.c_state != 0;
    let contract = r.pd_state != 0;
    let name = |s: &str| s.to_string();
    PdPortInfo {
        port,
        connected,
        pd_contract: contract,
        power_role: name(match r.power_role {
            0 => "Sink",
            1 => "Source",
            _ => "Unknown",
        }),
        data_role: name(match r.data_role {
            0 => "Ufp",
            1 => "Dfp",
            2 => "Disconnected",
            _ => "Unknown",
        }),
        vconn: connected.then_some(r.vconn != 0),
        voltage_mv: connected.then_some(u32::from(r.voltage_mv)),
        current_ma: connected.then_some(u32::from(r.current_ma)),
        cc_polarity: connected.then(|| {
            name(match r.cc_polarity {
                0 => "CC1",
                1 => "CC2",
                2 => "CC1 (Debug)",
                3 => "CC2 (Debug)",
                _ => "Unknown",
            })
        }),
        partner: contract.then(|| {
            name(match r.c_state {
                0 => "Nothing",
                1 => "Sink",
                2 => "Source",
                3 => "Debug",
                4 => "Audio",
                5 => "PoweredAccessory",
                6 => "Unsupported",
                _ => "Invalid",
            })
        }),
        epr_active: contract.then_some(r.epr_active != 0),
        epr_supported: contract.then_some(r.epr_support != 0),
        sink_active: (contract && r.power_role == 0).then_some(r.active_port != 0),
        // Bits 0-1 say DP alt mode is active; the other bits only mean something then.
        dp_alt_mode: if connected && r.alt_mode & 0x03 != 0 {
            DP_ALT_MODE_FLAGS
                .iter()
                .filter(|(bit, _)| r.alt_mode & bit != 0)
                .map(|(_, flag)| name(flag))
                .collect()
        } else {
            Vec::new()
        },
    }
}

/// Expansion bay status plus the two follow-up reads the CLI does.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExpansionBayRaw {
    pub state: u8,
    pub board_id_0: u8,
    pub board_id_1: u8,
    /// Serial bytes, when the EC says they are valid
    pub serial: Option<[u8; 20]>,
    /// (PCIe config, vendor), when the EC answered
    pub pcie: Option<(u8, u8)>,
}

pub fn expansion_bay_from_raw(r: &ExpansionBayRaw) -> ExpansionBayInfo {
    let known_or_number = |known: Option<&str>, raw: u8| match known {
        Some(s) => s.to_string(),
        None => format!("Unknown ({})", raw),
    };
    ExpansionBayInfo {
        enabled: r.state & 0x01 != 0,
        fault: r.state & 0x02 != 0,
        door_closed: r.state & 0x04 != 0,
        board: match (r.board_id_0, r.board_id_1) {
            (12, 12) => "DualInterposer".to_string(),
            (13, 15) => "UmaFans".to_string(),
            (11, 15) => "SingleInterposer".to_string(),
            (15, 15) => "NoModule".to_string(),
            (a, b) => format!("BadConnection({}, {})", a, b),
        },
        serial: r
            .serial
            .as_ref()
            .and_then(|bytes| clean_serial(&String::from_utf8_lossy(bytes))),
        pcie_config: r.pcie.map(|(config, _)| {
            known_or_number(
                match config {
                    0 => Some("Pcie8x1"),
                    1 => Some("Pcie4x1"),
                    2 => Some("Pcie4x2"),
                    _ => None,
                },
                config,
            )
        }),
        vendor: r.pcie.map(|(_, vendor)| {
            known_or_number(
                match vendor {
                    0 => Some("Initializing"),
                    1 => Some("FanOnly"),
                    2 => Some("GpuAmdR23M"),
                    3 => Some("SsdHolder"),
                    4 => Some("PcieAccessory"),
                    5 => Some("NvidiaGn22"),
                    _ => None,
                },
                vendor,
            )
        }),
    }
}

/// The serial is 18 characters in a 20-byte, NUL-padded field; the CLI prints "Unknown" without one.
fn clean_serial(s: &str) -> Option<String> {
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!s.is_empty() && s != "Unknown").then(|| s.to_string())
}

/// Number of board-id slots in the deck state response: 5 top-row positions, touchpad, unused, hub board.
pub const INPUT_DECK_SLOTS: usize = 8;
const TOP_ROW_SLOTS: usize = 5;
const TOUCHPAD_SLOT: usize = 5;

pub fn input_deck_from_raw(board_id: &[u8; INPUT_DECK_SLOTS], deck_state: u8) -> InputDeckInfo {
    let module = |id: u8| match id {
        0 => "Short".to_string(),
        1..=5 => format!("Reserved{}", id),
        6 => "FullWidth".to_string(),
        7 => "HubBoard".to_string(),
        8 => "GenericA".to_string(),
        9 => "GenericB".to_string(),
        10 => "GenericC".to_string(),
        11 => "KeyboardB".to_string(),
        12 => "KeyboardA".to_string(),
        13 => "Touchpad".to_string(),
        14 => "Reserved15".to_string(),
        15 => "Disconnected".to_string(),
        other => format!("Unknown ({})", other),
    };
    InputDeckInfo {
        state: match deck_state {
            0 => "Off".to_string(),
            1 => "Disconnected".to_string(),
            2 => "TurningOn".to_string(),
            3 => "On".to_string(),
            4 => "ForceOff".to_string(),
            5 => "ForceOn".to_string(),
            6 => "NoDetection".to_string(),
            other => format!("Unknown ({})", other),
        },
        // Short (0) and Disconnected (15) both mean no touchpad.
        touchpad_present: !matches!(board_id[TOUCHPAD_SLOT], 0 | 15),
        top_row: board_id[..TOP_ROW_SLOTS].iter().map(|id| module(*id)).collect(),
    }
}

/// The EC reports 1 for a connected (not cut off) device. For the camera that means powered,
/// see [`PrivacyInfo::camera_connected`].
pub fn privacy_from_raw(microphone: u8, camera: u8) -> PrivacyInfo {
    PrivacyInfo {
        microphone_connected: microphone == 1,
        camera_connected: camera == 1,
    }
}

/// `status` is the chassis-open check; the rest is the intrusion history.
pub fn intrusion_from_raw(
    status: u8,
    chassis_ever_opened: u8,
    coin_batt_ever_remove: u8,
    total_open_count: u8,
    vtr_open_count: u8,
) -> IntrusionInfo {
    IntrusionInfo {
        currently_open: status == 1,
        ever_opened: chassis_ever_opened == 1,
        coin_cell_ever_removed: coin_batt_ever_remove == 1,
        total_opened: u32::from(total_open_count),
        opened_while_off: u32::from(vtr_open_count),
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing framework_tool output (0.6.6 formats)

/// `Key: value` lines by key; the first occurrence of a key wins.
fn key_values(stdout: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once(':') {
            out.entry(key.trim().to_string())
                .or_insert_with(|| value.trim().to_string());
        }
    }
    out
}

/// "20.000 V" -> 20000
fn parse_volts_mv(s: &str) -> Option<u32> {
    let s = s.trim().strip_suffix('V')?.trim();
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let frac = format!("{:0<3}", frac);
    Some(whole.parse::<u32>().ok()? * 1000 + frac.get(..3)?.parse::<u32>().ok()?)
}

/// Parse `framework_tool --pdports`. `None` when no port is listed (the EC lacks the command).
pub fn parse_pd_ports(stdout: &str) -> Option<Vec<PdPortInfo>> {
    let mut ports: Vec<PdPortInfo> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("USB-C Port ") {
            // "USB-C Port 0:". Anything else is a format this parser doesn't know.
            let port = rest.strip_suffix(':')?.trim().parse::<u8>().ok()?;
            ports.push(PdPortInfo {
                port,
                ..Default::default()
            });
            continue;
        }
        let Some(p) = ports.last_mut() else { continue };
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "PD Contract" => p.pd_contract = value == "Yes",
            "Power Role" => p.power_role = value.to_string(),
            "Data Role" => p.data_role = value.to_string(),
            // Printed for connected ports only.
            "VCONN" => {
                p.connected = true;
                p.vconn = Some(value == "On");
            }
            // "20.000 V, 4630 mA, 92.600 W"
            "Negotiated" => {
                let mut parts = value.split(',');
                p.voltage_mv = parts.next().and_then(parse_volts_mv);
                p.current_ma = parts
                    .next()
                    .and_then(|s| s.trim().strip_suffix("mA"))
                    .and_then(|s| s.trim().parse::<u32>().ok());
            }
            "CC Polarity" => p.cc_polarity = Some(value.to_string()),
            "Port Partner" => p.partner = Some(value.to_string()),
            // "Active", "Inactive", each optionally followed by " (Supported)"
            "EPR" => {
                p.epr_active = Some(value.starts_with("Active"));
                p.epr_supported = Some(value.contains("(Supported)"));
            }
            "Sink Active" => p.sink_active = Some(value == "Yes"),
            // "UFP_D Connected, HPD High (0x82)"
            "DP Alt Mode" => {
                let flags = value.rsplit_once(" (0x").map(|(flags, _)| flags).unwrap_or(value);
                p.dp_alt_mode = flags
                    .split(',')
                    .map(str::trim)
                    .filter(|f| !f.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }
    (!ports.is_empty()).then_some(ports)
}

/// Parse `framework_tool --expansion-bay`. `None` without the status lines (no expansion bay).
pub fn parse_expansion_bay(stdout: &str) -> Option<ExpansionBayInfo> {
    let kv = key_values(stdout);
    let flag = |key: &str| kv.get(key).and_then(|v| v.parse::<bool>().ok());
    Some(ExpansionBayInfo {
        enabled: flag("Enabled")?,
        fault: !flag("No fault")?,
        door_closed: flag("Door closed")?,
        board: kv.get("Board")?.clone(),
        serial: kv.get("Serial Number").and_then(|s| clean_serial(s)),
        pcie_config: kv.get("Config").cloned(),
        vendor: kv.get("Vendor").cloned(),
    })
}

/// Parse `framework_tool --inputdeck` in its Framework 16 format. `None` for the other models'
/// formats, which describe different hardware.
pub fn parse_input_deck(stdout: &str) -> Option<InputDeckInfo> {
    let kv = key_values(stdout);
    let mut top_row = Vec::with_capacity(TOP_ROW_SLOTS);
    for pos in 0..TOP_ROW_SLOTS {
        top_row.push(kv.get(format!("Pos {}", pos).as_str())?.clone());
    }
    Some(InputDeckInfo {
        state: kv.get("Input Deck State")?.clone(),
        touchpad_present: kv.get("Touchpad present")?.parse::<bool>().ok()?,
        top_row,
    })
}

/// Parse `framework_tool --privacy`. `None` when the EC doesn't support it.
pub fn parse_privacy(stdout: &str) -> Option<PrivacyInfo> {
    let kv = key_values(stdout);
    Some(PrivacyInfo {
        microphone_connected: kv.get("Microphone")? == "Connected",
        camera_connected: kv.get("Camera")? == "Connected",
    })
}

/// Parse `framework_tool --intrusion`. `None` when it prints "Unable to tell".
pub fn parse_intrusion(stdout: &str) -> Option<IntrusionInfo> {
    let kv = key_values(stdout);
    let flag = |key: &str| kv.get(key).and_then(|v| v.parse::<bool>().ok());
    // "3 times"
    let count = |key: &str| {
        kv.get(key)
            .and_then(|v| v.split_whitespace().next())
            .and_then(|n| n.parse::<u32>().ok())
    };
    Some(IntrusionInfo {
        currently_open: flag("Chassis currently open")?,
        ever_opened: flag("Chassis ever opened")?,
        coin_cell_ever_removed: flag("Coin cell ever removed")?,
        total_opened: count("Chassis opened")?,
        opened_while_off: count("Chassis opened while off")?,
    })
}

// ---------------------------------------------------------------------------------------------
// FrameworkTool operations

// A host command takes about 15 ms on the Windows driver and the EC worker is shared with fan
// control, so nothing here is read more often than the UI can show it.
/// PD state changes when something is plugged in, so it is refreshed about as often as the UI polls.
const PD_PORTS_TTL: Duration = Duration::from_millis(2500);
const PRIVACY_TTL: Duration = Duration::from_secs(5);
/// The rest changes when the user takes the machine apart.
const SLOW_TTL: Duration = Duration::from_secs(15);

impl FrameworkTool {
    pub async fn pd_ports(&self) -> Result<Option<Vec<PdPortInfo>>, String> {
        global_cache::cache_get_or_update("framework_tool.pd_ports", PD_PORTS_TTL, true, || async {
            self.exec(
                "pd_ports",
                |ec| ec.pd_ports(),
                || async {
                    let out = self.run_cli(&["--pdports"]).await?;
                    Ok(parse_pd_ports(&out))
                },
            )
            .await
        })
        .await
    }

    pub async fn expansion_bay(&self) -> Result<Option<ExpansionBayInfo>, String> {
        global_cache::cache_get_or_update("framework_tool.expansion_bay", SLOW_TTL, true, || async {
            self.exec(
                "expansion_bay",
                |ec| ec.expansion_bay(),
                || async {
                    let out = self.run_cli(&["--expansion-bay"]).await?;
                    Ok(parse_expansion_bay(&out))
                },
            )
            .await
        })
        .await
    }

    pub async fn input_deck(&self) -> Result<Option<InputDeckInfo>, String> {
        global_cache::cache_get_or_update("framework_tool.input_deck", SLOW_TTL, true, || async {
            self.exec(
                "input_deck",
                |ec| ec.input_deck(),
                || async {
                    let out = self.run_cli(&["--inputdeck"]).await?;
                    Ok(parse_input_deck(&out))
                },
            )
            .await
        })
        .await
    }

    pub async fn privacy(&self) -> Result<Option<PrivacyInfo>, String> {
        global_cache::cache_get_or_update("framework_tool.privacy", PRIVACY_TTL, true, || async {
            self.exec(
                "privacy",
                |ec| ec.privacy(),
                || async {
                    let out = self.run_cli(&["--privacy"]).await?;
                    Ok(parse_privacy(&out))
                },
            )
            .await
        })
        .await
    }

    pub async fn intrusion(&self) -> Result<Option<IntrusionInfo>, String> {
        global_cache::cache_get_or_update("framework_tool.intrusion", SLOW_TTL, true, || async {
            self.exec(
                "intrusion",
                |ec| ec.intrusion(),
                || async {
                    let out = self.run_cli(&["--intrusion"]).await?;
                    Ok(parse_intrusion(&out))
                },
            )
            .await
        })
        .await
    }

    /// Every hardware view. A view that can't be read is left out, like one this machine lacks.
    pub async fn hardware_info(&self) -> HardwareInfo {
        HardwareInfo {
            pd_ports: readable("pd_ports", self.pd_ports().await),
            expansion_bay: readable("expansion_bay", self.expansion_bay().await),
            input_deck: readable("input_deck", self.input_deck().await),
            privacy: readable("privacy", self.privacy().await),
            intrusion: readable("intrusion", self.intrusion().await),
        }
    }
}

fn readable<T>(view: &str, result: Result<Option<T>, String>) -> Option<T> {
    result.unwrap_or_else(|e| {
        debug!("hardware info: {} unavailable: {}", view, e);
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PD ports ------------------------------------------------------------------------------

    /// `framework_tool --pdports` on a Framework 16 (0.6.6): charger and display on port 0,
    /// a non-PD device on port 3.
    const PD_PORTS_FW16: &str = "\
USB-C Port 0:
  PD Contract:   Yes
  Power Role:    Sink
  Data Role:     Dfp
  VCONN:         On
  Negotiated:    20.000 V, 4630 mA, 92.600 W
  CC Polarity:   CC1
  Port Partner:  Source
  EPR:           Inactive
  Sink Active:   Yes
  DP Alt Mode:   UFP_D Connected, HPD High (0x82)
USB-C Port 1:
  PD Contract:   No
  Power Role:    Sink
  Data Role:     Ufp
USB-C Port 2:
  PD Contract:   No
  Power Role:    Sink
  Data Role:     Ufp
USB-C Port 3:
  PD Contract:   No
  Power Role:    Source
  Data Role:     Dfp
  VCONN:         Off
  Negotiated:    5.000 V, 1500 mA, 7.500 W
  CC Polarity:   CC2
";

    fn charger_port() -> PdPortRaw {
        PdPortRaw {
            c_state: 2,
            pd_state: 1,
            power_role: 0,
            data_role: 1,
            vconn: 1,
            epr_active: 0,
            epr_support: 0,
            cc_polarity: 0,
            voltage_mv: 20000,
            current_ma: 4630,
            active_port: 1,
            alt_mode: 0x82,
        }
    }

    fn empty_port() -> PdPortRaw {
        PdPortRaw::default()
    }

    fn non_pd_device_port() -> PdPortRaw {
        PdPortRaw {
            c_state: 1,
            power_role: 1,
            data_role: 1,
            cc_polarity: 1,
            voltage_mv: 5000,
            current_ma: 1500,
            ..Default::default()
        }
    }

    #[test]
    fn pd_ports_parse_captured_output() {
        let ports = parse_pd_ports(PD_PORTS_FW16).unwrap();
        assert_eq!(ports.len(), 4);
        assert_eq!(
            ports[0],
            PdPortInfo {
                port: 0,
                connected: true,
                pd_contract: true,
                power_role: "Sink".into(),
                data_role: "Dfp".into(),
                vconn: Some(true),
                voltage_mv: Some(20000),
                current_ma: Some(4630),
                cc_polarity: Some("CC1".into()),
                partner: Some("Source".into()),
                epr_active: Some(false),
                epr_supported: Some(false),
                sink_active: Some(true),
                dp_alt_mode: vec!["UFP_D Connected".into(), "HPD High".into()],
            }
        );
        assert_eq!(
            ports[1],
            PdPortInfo {
                port: 1,
                power_role: "Sink".into(),
                data_role: "Ufp".into(),
                ..Default::default()
            }
        );
        assert_eq!(ports[3].port, 3);
        assert!(ports[3].connected && !ports[3].pd_contract);
        assert_eq!(ports[3].voltage_mv, Some(5000));
        assert_eq!(ports[3].current_ma, Some(1500));
        assert_eq!(ports[3].vconn, Some(false));
        assert_eq!(ports[3].partner, None);
    }

    #[test]
    fn pd_ports_from_raw_match_captured_output() {
        let ours: Vec<PdPortInfo> = [charger_port(), empty_port(), empty_port(), non_pd_device_port()]
            .iter()
            .enumerate()
            .map(|(i, raw)| pd_port_from_raw(i as u8, raw))
            .collect();
        assert_eq!(Some(ours), parse_pd_ports(PD_PORTS_FW16));
    }

    /// Render `framework_tool --pdports` (0.6.6 `get_and_print_cypd_pd_info`) for raw port states.
    fn render_cli_pd_ports(ports: &[PdPortRaw]) -> String {
        let mut s = String::new();
        for (port, r) in ports.iter().enumerate() {
            let connected = r.c_state != 0;
            let contract = r.pd_state != 0;
            s += &format!("USB-C Port {}:\n", port);
            s += &format!("  PD Contract:   {}\n", if contract { "Yes" } else { "No" });
            s += &format!(
                "  Power Role:    {}\n",
                ["Sink", "Source"].get(r.power_role as usize).unwrap_or(&"Unknown")
            );
            s += &format!(
                "  Data Role:     {}\n",
                ["Ufp", "Dfp", "Disconnected"]
                    .get(r.data_role as usize)
                    .unwrap_or(&"Unknown")
            );
            if connected {
                let mw = r.voltage_mv as u32 * r.current_ma as u32 / 1000;
                s += &format!("  VCONN:         {}\n", if r.vconn != 0 { "On" } else { "Off" });
                s += &format!(
                    "  Negotiated:    {}.{:03} V, {} mA, {}.{} W\n",
                    r.voltage_mv / 1000,
                    r.voltage_mv % 1000,
                    r.current_ma,
                    mw / 1000,
                    mw % 1000
                );
                s += &format!(
                    "  CC Polarity:   {}\n",
                    ["CC1", "CC2", "CC1 (Debug)", "CC2 (Debug)"]
                        .get(r.cc_polarity as usize)
                        .unwrap_or(&"Unknown")
                );
            }
            if contract {
                s += &format!(
                    "  Port Partner:  {}\n",
                    [
                        "Nothing",
                        "Sink",
                        "Source",
                        "Debug",
                        "Audio",
                        "PoweredAccessory",
                        "Unsupported"
                    ]
                    .get(r.c_state as usize)
                    .unwrap_or(&"Invalid")
                );
                s += &format!(
                    "  EPR:           {}{}\n",
                    if r.epr_active != 0 { "Active" } else { "Inactive" },
                    if r.epr_support != 0 { " (Supported)" } else { "" }
                );
                if r.power_role == 0 {
                    s += &format!("  Sink Active:   {}\n", if r.active_port != 0 { "Yes" } else { "No" });
                }
            }
            if connected && r.alt_mode & 0x03 != 0 {
                let names = [
                    "DFP_D Connected",
                    "UFP_D Connected",
                    "Power Low",
                    "Enabled",
                    "Multi-Function",
                    "USB Config",
                    "Exit Request",
                    "HPD High",
                ];
                let flags: Vec<&str> = (0..8)
                    .filter(|bit| r.alt_mode & (1 << bit) != 0)
                    .map(|bit| names[bit])
                    .collect();
                s += &format!("  DP Alt Mode:   {} (0x{:02X})\n", flags.join(", "), r.alt_mode);
            }
        }
        s
    }

    #[test]
    fn pd_ports_from_raw_match_parsed_cli_output() {
        let epr_charger = PdPortRaw {
            epr_active: 1,
            epr_support: 1,
            voltage_mv: 36000,
            current_ma: 5000,
            alt_mode: 0,
            ..charger_port()
        };
        // A sink with a contract that isn't the active charging port, 9.050 V.
        let second_charger = PdPortRaw {
            epr_support: 1,
            cc_polarity: 3,
            voltage_mv: 9050,
            current_ma: 3000,
            active_port: 0,
            alt_mode: 0x80, // HPD without DP alt mode bits: not printed
            ..charger_port()
        };
        // Sourcing power to a PD device, with a display in DFP_D mode.
        let dock = PdPortRaw {
            c_state: 1,
            pd_state: 1,
            power_role: 1,
            data_role: 1,
            vconn: 1,
            voltage_mv: 5000,
            current_ma: 3000,
            alt_mode: 0x99,
            ..Default::default()
        };
        // Values the library has no name for.
        let odd = PdPortRaw {
            c_state: 9,
            pd_state: 1,
            power_role: 7,
            data_role: 7,
            cc_polarity: 9,
            ..Default::default()
        };
        let raws = [epr_charger, second_charger, dock, odd];
        let ours: Vec<PdPortInfo> = raws
            .iter()
            .enumerate()
            .map(|(i, raw)| pd_port_from_raw(i as u8, raw))
            .collect();
        assert_eq!(Some(ours.clone()), parse_pd_ports(&render_cli_pd_ports(&raws)));
        assert_eq!(ours[0].epr_active, Some(true));
        assert_eq!(ours[1].voltage_mv, Some(9050));
        assert_eq!(ours[1].sink_active, Some(false));
        assert!(ours[1].dp_alt_mode.is_empty());
        assert_eq!(
            ours[2].dp_alt_mode,
            vec!["DFP_D Connected", "Enabled", "Multi-Function", "HPD High"]
        );
        assert_eq!(ours[2].sink_active, None);
        assert_eq!(ours[3].partner.as_deref(), Some("Invalid"));
    }

    #[test]
    fn pd_ports_absent_when_nothing_is_listed() {
        // An EC without the command: the CLI logs errors to stderr and prints nothing.
        assert_eq!(parse_pd_ports(""), None);
        // `--pdports-chromebook` output is a different report.
        assert_eq!(
            parse_pd_ports("USB-C Port 0 (Right Back):\n  Role:          Sink\n"),
            None
        );
    }

    // ---- expansion bay -------------------------------------------------------------------------

    /// `framework_tool --expansion-bay` on a Framework 16 with a GPU module (0.6.6).
    const EXPANSION_BAY_GPU: &str = "\
Expansion Bay
  Enabled:       true
  No fault:      true
  Door closed:   true
  Board:         DualInterposer
  Serial Number: FRAKJECPA1539300K4\0\0
  Config:        Pcie8x1
  Vendor:        NvidiaGn22
  Expansion Bay EEPROM
    Valid:       true
    HW Version:  4.0
";

    fn serial_bytes(serial: &str) -> [u8; 20] {
        let mut bytes = [0u8; 20];
        bytes[..serial.len()].copy_from_slice(serial.as_bytes());
        bytes
    }

    #[test]
    fn expansion_bay_from_raw_matches_captured_output() {
        let raw = ExpansionBayRaw {
            state: 0x05,
            board_id_0: 12,
            board_id_1: 12,
            serial: Some(serial_bytes("FRAKJECPA1539300K4")),
            pcie: Some((0, 5)),
        };
        let ours = expansion_bay_from_raw(&raw);
        assert_eq!(Some(ours.clone()), parse_expansion_bay(EXPANSION_BAY_GPU));
        assert_eq!(
            ours,
            ExpansionBayInfo {
                enabled: true,
                fault: false,
                door_closed: true,
                board: "DualInterposer".into(),
                serial: Some("FRAKJECPA1539300K4".into()),
                pcie_config: Some("Pcie8x1".into()),
                vendor: Some("NvidiaGn22".into()),
            }
        );
    }

    /// Render `framework_tool --expansion-bay` (0.6.6 `check_bay_status`).
    fn render_cli_expansion_bay(r: &ExpansionBayRaw) -> String {
        let mut s = String::from("Expansion Bay\n");
        s += &format!("  Enabled:       {}\n", r.state & 1 != 0);
        s += &format!("  No fault:      {}\n", r.state & 2 == 0);
        s += &format!("  Door closed:   {}\n", r.state & 4 != 0);
        let board = match (r.board_id_0, r.board_id_1) {
            (12, 12) => "DualInterposer".to_string(),
            (13, 15) => "UmaFans".to_string(),
            (11, 15) => "SingleInterposer".to_string(),
            (15, 15) => "NoModule".to_string(),
            (a, b) => format!("BadConnection({}, {})", a, b),
        };
        s += &format!("  Board:         {}\n", board);
        match &r.serial {
            Some(bytes) => s += &format!("  Serial Number: {}\n", String::from_utf8(bytes.to_vec()).unwrap()),
            None => s += "  Serial Number: Unknown\n",
        }
        // Without an answer to the PCIe query the CLI stops here.
        if let Some((config, vendor)) = r.pcie {
            match ["Pcie8x1", "Pcie4x1", "Pcie4x2"].get(config as usize) {
                Some(name) => s += &format!("  Config:        {}\n", name),
                None => s += &format!("  Config:        Unknown ({})\n", config),
            }
            match [
                "Initializing",
                "FanOnly",
                "GpuAmdR23M",
                "SsdHolder",
                "PcieAccessory",
                "NvidiaGn22",
            ]
            .get(vendor as usize)
            {
                Some(name) => s += &format!("  Vendor:        {}\n", name),
                None => s += &format!("  Vendor:        Unknown ({})\n", vendor),
            }
        }
        s
    }

    #[test]
    fn expansion_bay_from_raw_matches_parsed_cli_output() {
        let cases = [
            // Fan-only shell, door open, no serial.
            ExpansionBayRaw {
                state: 0x01,
                board_id_0: 13,
                board_id_1: 15,
                serial: None,
                pcie: Some((1, 1)),
            },
            // Faulted AMD GPU module.
            ExpansionBayRaw {
                state: 0x06,
                board_id_0: 12,
                board_id_1: 12,
                serial: Some(serial_bytes("FRAGMASP81331500X9")),
                pcie: Some((0, 2)),
            },
            // Nothing in the bay.
            ExpansionBayRaw {
                state: 0x00,
                board_id_0: 15,
                board_id_1: 15,
                serial: None,
                pcie: Some((2, 0)),
            },
            // Interposer not seated, values newer than the library, PCIe query unanswered.
            ExpansionBayRaw {
                state: 0x07,
                board_id_0: 12,
                board_id_1: 3,
                serial: Some(serial_bytes("")),
                pcie: Some((7, 9)),
            },
            ExpansionBayRaw {
                state: 0x05,
                board_id_0: 11,
                board_id_1: 15,
                serial: None,
                pcie: None,
            },
        ];
        for raw in &cases {
            assert_eq!(
                Some(expansion_bay_from_raw(raw)),
                parse_expansion_bay(&render_cli_expansion_bay(raw)),
                "{raw:?}"
            );
        }
        assert_eq!(expansion_bay_from_raw(&cases[3]).board, "BadConnection(12, 3)");
        assert_eq!(expansion_bay_from_raw(&cases[3]).serial, None);
        assert_eq!(expansion_bay_from_raw(&cases[3]).vendor.as_deref(), Some("Unknown (9)"));
        assert!(expansion_bay_from_raw(&cases[1]).fault);
    }

    #[test]
    fn expansion_bay_absent_without_status() {
        // Models without an expansion bay: the header, then an error on stderr.
        assert_eq!(parse_expansion_bay("Expansion Bay\n"), None);
    }

    // ---- input deck ----------------------------------------------------------------------------

    /// `framework_tool --inputdeck` on a Framework 16 (0.6.6).
    const INPUT_DECK_FW16: &str = "\
Chassis Closed:   true
Input Deck State: On
Touchpad present: true
SLEEP# GPIO high: true
Positions:
  Pos 0: KeyboardA
  Pos 1: Disconnected
  Pos 2: Disconnected
  Pos 3: KeyboardB
  Pos 4: Disconnected
";

    #[test]
    fn input_deck_from_raw_matches_captured_output() {
        let ours = input_deck_from_raw(&[12, 15, 15, 11, 15, 13, 15, 7], 3);
        assert_eq!(Some(ours.clone()), parse_input_deck(INPUT_DECK_FW16));
        assert_eq!(ours.state, "On");
        assert!(ours.touchpad_present);
        assert_eq!(
            ours.top_row,
            vec!["KeyboardA", "Disconnected", "Disconnected", "KeyboardB", "Disconnected"]
        );
    }

    #[test]
    fn input_deck_names_every_state_and_module() {
        let states = [
            "Off",
            "Disconnected",
            "TurningOn",
            "On",
            "ForceOff",
            "ForceOn",
            "NoDetection",
        ];
        for (raw, name) in states.iter().enumerate() {
            assert_eq!(input_deck_from_raw(&[15; 8], raw as u8).state, *name);
        }
        let deck = input_deck_from_raw(&[0, 6, 8, 9, 10, 15, 15, 7], 1);
        assert_eq!(
            deck.top_row,
            vec!["Short", "FullWidth", "GenericA", "GenericB", "GenericC"]
        );
        assert!(!deck.touchpad_present);
        assert!(!input_deck_from_raw(&[15, 15, 15, 15, 15, 0, 15, 7], 3).touchpad_present);
        // Values the library panics on are reported instead.
        let odd = input_deck_from_raw(&[99, 15, 15, 15, 15, 13, 15, 7], 42);
        assert_eq!(odd.top_row[0], "Unknown (99)");
        assert_eq!(odd.state, "Unknown (42)");
    }

    #[test]
    fn input_deck_absent_for_other_models_output() {
        // Framework 13 (0.6.6 `print_fw13_inputdeck_status`).
        let fw13 = "\
Input Deck
  Chassis Closed:      true
  Audio Daughterboard: Present (7)
  Touchpad:            Present (7)
  Deck State:          On
  Touchpad present:    true
";
        assert_eq!(parse_input_deck(fw13), None);
        assert_eq!(parse_input_deck("  Unable to tell\n"), None);
    }

    // ---- privacy, intrusion --------------------------------------------------------------------

    #[test]
    fn privacy_matches_cli_output() {
        let render = |mic: u8, cam: u8| {
            let word = |v: u8| if v == 1 { "Connected" } else { "Disconnected" };
            format!(
                "Privacy Slider (Black = Device Connected; Red = Device Disconnected)\n  Microphone:  {}\n  Camera:      {}\n",
                word(mic),
                word(cam)
            )
        };
        for (mic, cam) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert_eq!(Some(privacy_from_raw(mic, cam)), parse_privacy(&render(mic, cam)));
        }
        assert!(privacy_from_raw(1, 0).microphone_connected);
        assert!(!privacy_from_raw(1, 0).camera_connected);
        assert_eq!(parse_privacy("Not all EC versions support this comand.\n"), None);
    }

    #[test]
    fn intrusion_matches_cli_output() {
        let render = |status: u8, ever: u8, coin: u8, total: u8, vtr: u8| {
            format!(
                "Chassis status:\n  Coin cell ever removed:   {}\n  Chassis currently open:   {}\n  Chassis ever opened:      {}\n  Chassis opened:           {} times\n  Chassis opened while off: {} times\n",
                coin == 1,
                status == 1,
                ever == 1,
                total,
                vtr
            )
        };
        for (status, ever, coin, total, vtr) in [(0, 0, 0, 0, 0), (1, 1, 0, 7, 1), (0, 1, 1, 255, 3)] {
            assert_eq!(
                Some(intrusion_from_raw(status, ever, coin, total, vtr)),
                parse_intrusion(&render(status, ever, coin, total, vtr))
            );
        }
        let opened = intrusion_from_raw(1, 1, 0, 7, 1);
        assert!(opened.currently_open && opened.ever_opened && !opened.coin_cell_ever_removed);
        assert_eq!((opened.total_opened, opened.opened_while_off), (7, 1));
        assert_eq!(parse_intrusion("Chassis status:\n  Unable to tell\n"), None);
    }
}
