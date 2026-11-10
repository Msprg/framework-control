//! `framework-control-service --ec-selftest [path/to/framework_tool] [--write]`
//!
//! Compares the in-process EC backend with the `framework_tool` CLI on this machine and times
//! both. Read-only unless `--write` is given, which writes the *current* charge limit back
//! through the in-process backend and checks it reads back unchanged.
//! Also checks that every sensor named in the saved fan config is reported in-process.
//! Exit code: 0 when everything matches, 1 on mismatches, 2 when it couldn't run.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::ec_lib;
use super::framework_tool::{default_cli_path, FrameworkTool};
use super::framework_tool_parser::{
    parse_charge_limit, parse_power, parse_thermal, PowerBatteryInfo, ThermalParsed, VersionsParsed,
};

struct Report {
    failures: u32,
}

impl Report {
    fn ok(&self, what: &str) {
        println!("  ok    {what}");
    }
    fn note(&self, what: &str) {
        println!("  note  {what}");
    }
    fn fail(&mut self, what: &str) {
        self.failures += 1;
        println!("  FAIL  {what}");
    }
    fn check(&mut self, cond: bool, what: &str) {
        if cond {
            self.ok(what)
        } else {
            self.fail(what)
        }
    }
}

pub async fn run(args: &[String]) -> i32 {
    let write = args.iter().any(|a| a == "--write");
    let explicit = args.iter().find(|a| !a.starts_with("--")).cloned();
    println!("framework-control EC self-test{}", if write { "" } else { " (read-only)" });

    let Some(path) = explicit.or_else(default_cli_path) else {
        println!("  FAIL  no framework_tool found; pass its path");
        return 2;
    };
    let cli = FrameworkTool::unchecked(path.clone());
    let cli_version = cli.cli_versions().await.ok().and_then(|v| v.tool_version);
    println!("CLI:        {} (framework_tool {})", path, cli_version.as_deref().unwrap_or("?"));
    println!("in-process: {}", ec_lib::label());

    let mut r = Report { failures: 0 };

    // ---- thermal ------------------------------------------------------------------------------
    println!("\n[thermal]");
    let mut attempt = 0;
    let (lib_t, cli_t) = loop {
        attempt += 1;
        let lib_t = match ec_lib::call("thermal", |ec| ec.thermal()).await {
            Ok(t) => t,
            Err(e) => {
                r.fail(&format!("in-process thermal: {e} (backend {})", ec_lib::status().await));
                return 2;
            }
        };
        let cli_t = match cli.run_cli(&["--thermal"]).await {
            Ok(out) => parse_thermal(&out),
            Err(e) => {
                r.fail(&format!("framework_tool --thermal: {e}"));
                return 2;
            }
        };
        // Sensors can appear/disappear between the two reads (e.g. dGPU power state); retry.
        if keys(&lib_t) == keys(&cli_t) || attempt >= 3 {
            break (lib_t, cli_t);
        }
    };
    compare_thermal(&mut r, &lib_t, &cli_t);
    check_config_sensors(&mut r, &lib_t, &cli_t);

    // ---- power --------------------------------------------------------------------------------
    println!("\n[power]");
    let lib_p = ec_lib::call("power", |ec| ec.power()).await;
    let cli_p = cli.run_cli(&["--power", "-vv"]).await.map(|o| parse_power(&o));
    match (lib_p, cli_p) {
        (Ok(l), Ok(c)) => compare_power(&mut r, &l, &c),
        (Err(e), Err(c)) => r.note(&format!("both report no power info (in-process: {e}; CLI: {c})")),
        (l, c) => r.fail(&format!(
            "only one backend answered: in-process {:?} / CLI {:?}",
            l.err(),
            c.err()
        )),
    }

    // ---- charge limit -------------------------------------------------------------------------
    println!("\n[charge limit]");
    let lib_cl = ec_lib::call("charge_limit_get", |ec| ec.charge_limit_get()).await;
    let cli_cl = cli.run_cli(&["--charge-limit"]).await.map(|o| parse_charge_limit(&o));
    match (&lib_cl, &cli_cl) {
        (Ok(l), Ok(c)) => r.check(
            l.charge_limit_min_pct == c.charge_limit_min_pct && l.charge_limit_max_pct == c.charge_limit_max_pct,
            &format!(
                "min/max {:?}/{:?} (CLI {:?}/{:?})",
                l.charge_limit_min_pct, l.charge_limit_max_pct, c.charge_limit_min_pct, c.charge_limit_max_pct
            ),
        ),
        (l, c) => r.fail(&format!("in-process {:?} / CLI {:?}", l.as_ref().err(), c.as_ref().err())),
    }
    if write {
        match lib_cl.as_ref().ok().and_then(|l| l.charge_limit_max_pct) {
            Some(max) => {
                let set = ec_lib::call("charge_limit_set", move |ec| ec.charge_limit_set(max)).await;
                let after = ec_lib::call("charge_limit_get", |ec| ec.charge_limit_get()).await;
                let before = lib_cl.as_ref().ok();
                let unchanged = after.as_ref().ok().map(|a| (a.charge_limit_min_pct, a.charge_limit_max_pct))
                    == before.map(|b| (b.charge_limit_min_pct, b.charge_limit_max_pct));
                r.check(
                    set.is_ok() && unchanged,
                    &format!("write current max {max}% back in-process: {:?}, reads back {:?}", set, after.ok()),
                );
            }
            None => r.note("no current charge limit to write back"),
        }
    }

    // ---- versions -----------------------------------------------------------------------------
    println!("\n[versions]");
    let lib_v = ec_lib::call("versions", |ec| ec.versions()).await;
    let cli_v = cli.cli_versions().await;
    match (lib_v, cli_v) {
        (Ok(l), Ok(c)) => compare_versions(&mut r, &l, &c),
        (l, c) => r.fail(&format!("in-process {:?} / CLI {:?}", l.err(), c.err())),
    }

    // ---- timing -------------------------------------------------------------------------------
    println!("\n[timing]");
    let cli_ref = &cli;
    let lib_thermal = time_n(50, || async { ec_lib::call("thermal", |ec| ec.thermal()).await.is_ok() }).await;
    let cli_thermal = time_n(10, move || async move { cli_ref.run_cli(&["--thermal"]).await.is_ok() }).await;
    let lib_power = time_n(20, || async { ec_lib::call("power", |ec| ec.power()).await.is_ok() }).await;
    let cli_power = time_n(5, move || async move { cli_ref.run_cli(&["--power", "-vv"]).await.is_ok() }).await;
    print_timing("thermal in-process", &lib_thermal);
    print_timing("thermal CLI", &cli_thermal);
    print_timing("power in-process", &lib_power);
    print_timing("power CLI", &cli_power);

    println!("\nbackend status: {}", ec_lib::status().await);
    if r.failures == 0 {
        println!("RESULT: in-process backend matches framework_tool");
        0
    } else {
        println!("RESULT: {} mismatch(es)", r.failures);
        1
    }
}

fn keys(t: &ThermalParsed) -> BTreeSet<String> {
    t.temps.keys().cloned().collect()
}

fn compare_thermal(r: &mut Report, lib: &ThermalParsed, cli: &ThermalParsed) {
    let (lk, ck) = (keys(lib), keys(cli));
    r.check(
        lk == ck,
        &format!(
            "sensor names: {} in-process, {} CLI{}",
            lk.len(),
            ck.len(),
            if lk == ck {
                String::new()
            } else {
                format!(
                    " (only in-process: {:?}; only CLI: {:?})",
                    lk.difference(&ck).collect::<Vec<_>>(),
                    ck.difference(&lk).collect::<Vec<_>>()
                )
            }
        ),
    );
    for (name, lv) in &lib.temps {
        if let Some(cv) = cli.temps.get(name) {
            r.check(
                (lv - cv).abs() <= 3,
                &format!("{name}: {lv} °C (CLI {cv} °C)"),
            );
        }
    }
    let lf: Vec<&str> = lib.fans.iter().map(|f| f.name.as_str()).collect();
    let cf: Vec<&str> = cli.fans.iter().map(|f| f.name.as_str()).collect();
    r.check(lf == cf, &format!("fan names {:?} (CLI {:?})", lf, cf));
    for (l, c) in lib.fans.iter().zip(cli.fans.iter()) {
        let tol = (c.rpm / 5).max(400);
        r.check(
            l.rpm.abs_diff(c.rpm) <= tol,
            &format!("{}: {} RPM (CLI {} RPM)", l.name, l.rpm, c.rpm),
        );
    }
}

/// Every sensor named in the saved fan config must be reported in-process whenever the CLI reports it.
fn check_config_sensors(r: &mut Report, lib: &ThermalParsed, cli: &ThermalParsed) {
    let path = crate::config::config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        r.note(&format!("no config at {} to check sensor names against", path.display()));
        return;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        r.note("config is not valid JSON; skipped sensor-name check");
        return;
    };
    let mut names = BTreeSet::new();
    collect_sensors(json.get("fan").unwrap_or(&serde_json::Value::Null), &mut names);
    if names.is_empty() {
        r.note("no sensors selected in the fan config");
        return;
    }
    for name in names {
        if lib.temps.contains_key(&name) {
            r.ok(&format!("config sensor {name} reported in-process"));
        } else if cli.temps.contains_key(&name) {
            r.fail(&format!("config sensor {name} reported by the CLI but not in-process"));
        } else {
            r.note(&format!("config sensor {name} not reported by either backend right now"));
        }
    }
}

fn collect_sensors(v: &serde_json::Value, out: &mut BTreeSet<String>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map {
                if k == "sensors" {
                    if let Some(arr) = child.as_array() {
                        out.extend(arr.iter().filter_map(|s| s.as_str().map(str::to_string)));
                    }
                } else {
                    collect_sensors(child, out);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|i| collect_sensors(i, out)),
        _ => {}
    }
}

/// framework_tool 0.6.6 prints voltages as `{}.{} V` without zero padding (16051 mV -> "16.51 V").
fn cli_voltage_rendering(mv: u32) -> u32 {
    format!("{}.{}", mv / 1000, mv % 1000)
        .parse::<f32>()
        .map(|v| (v * 1000.0) as u32)
        .unwrap_or(mv)
}

fn voltage_matches(lib: Option<u32>, cli: Option<u32>, tol: u32) -> bool {
    match (lib, cli) {
        (Some(l), Some(c)) => l.abs_diff(c) <= tol || cli_voltage_rendering(l).abs_diff(c) <= tol,
        (l, c) => l == c,
    }
}

fn compare_power(r: &mut Report, l: &PowerBatteryInfo, c: &PowerBatteryInfo) {
    r.check(
        l.ac_present == c.ac_present && l.battery_present == c.battery_present,
        &format!(
            "ac {:?} battery {:?} (CLI {:?} {:?})",
            l.ac_present, l.battery_present, c.ac_present, c.battery_present
        ),
    );
    for (name, lv, cv) in [
        ("full charge capacity", l.last_full_charge_capacity_mah, c.last_full_charge_capacity_mah),
        ("design capacity", l.design_capacity_mah, c.design_capacity_mah),
        ("cycle count", l.cycle_count, c.cycle_count),
    ] {
        r.check(lv == cv, &format!("{name}: {:?} (CLI {:?})", lv, cv));
    }
    r.check(
        voltage_matches(l.design_voltage_mv, c.design_voltage_mv, 0),
        &format!("design voltage: {:?} mV (CLI {:?})", l.design_voltage_mv, c.design_voltage_mv),
    );
    let close = |a: Option<u32>, b: Option<u32>, tol: u32| match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= tol,
        (a, b) => a == b,
    };
    r.check(
        close(l.remaining_capacity_mah, c.remaining_capacity_mah, 50),
        &format!("remaining: {:?} mAh (CLI {:?})", l.remaining_capacity_mah, c.remaining_capacity_mah),
    );
    r.check(
        close(l.percentage, c.percentage, 1),
        &format!("charge level: {:?}% (CLI {:?})", l.percentage, c.percentage),
    );
    r.check(
        close(l.soc_pct, c.soc_pct, 1),
        &format!("SoC: {:?}% (CLI {:?})", l.soc_pct, c.soc_pct),
    );
    r.check(
        voltage_matches(l.present_voltage_mv, c.present_voltage_mv, 300),
        &format!("voltage: {:?} mV (CLI {:?})", l.present_voltage_mv, c.present_voltage_mv),
    );
    r.check(
        l.charging == c.charging && l.discharging == c.discharging,
        &format!(
            "charging {:?} discharging {:?} (CLI {:?} {:?})",
            l.charging, l.discharging, c.charging, c.discharging
        ),
    );
    r.check(
        l.charger_voltage_mv.is_some() == c.charger_voltage_mv.is_some(),
        &format!(
            "charger: {:?} mV {:?} mA, input {:?} mA (CLI {:?} mV {:?} mA, input {:?} mA)",
            l.charger_voltage_mv,
            l.charger_current_ma,
            l.charge_input_current_ma,
            c.charger_voltage_mv,
            c.charger_current_ma,
            c.charge_input_current_ma
        ),
    );
    r.note(&format!("rate: {:?} mA (CLI {:?})", l.present_rate_ma, c.present_rate_ma));
}

fn compare_versions(r: &mut Report, l: &VersionsParsed, c: &VersionsParsed) {
    for (name, lv, cv) in [
        ("mainboard type", &l.mainboard_type, &c.mainboard_type),
        ("mainboard revision", &l.mainboard_revision, &c.mainboard_revision),
        ("UEFI version", &l.uefi_version, &c.uefi_version),
        ("UEFI release date", &l.uefi_release_date, &c.uefi_release_date),
        ("EC build version", &l.ec_build_version, &c.ec_build_version),
        ("EC current image", &l.ec_current_image, &c.ec_current_image),
    ] {
        r.check(lv == cv, &format!("{name}: {:?} (CLI {:?})", lv, cv));
    }
}

async fn time_n<F, Fut>(n: usize, mut f: F) -> Vec<Duration>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        if f().await {
            out.push(start.elapsed());
        }
    }
    out.sort();
    out
}

fn print_timing(label: &str, samples: &[Duration]) {
    if samples.is_empty() {
        println!("  {label:<20} no successful samples");
        return;
    }
    let pick = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "  {label:<20} n={:<3} min {:>7.2} ms  p50 {:>7.2} ms  p90 {:>7.2} ms",
        samples.len(),
        ms(samples[0]),
        ms(pick(0.5)),
        ms(pick(0.9))
    );
}
