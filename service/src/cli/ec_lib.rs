//! Build probe for the in-process EC backend (replaced by the real adapter).
#![allow(dead_code)]

use framework_lib::chromium_ec::commands::{ChargeStateCmd, EcRequestChargeStateGetV0};
use framework_lib::chromium_ec::{CrosEc, CrosEcDriver, CrosEcDriverType, EcCurrentImage, EcRequestRaw};

fn api_surface() -> Option<()> {
    #[cfg(windows)]
    let driver = CrosEcDriverType::Windows;
    #[cfg(target_os = "linux")]
    let driver = CrosEcDriverType::CrosEc;
    let ec = CrosEc::with(driver)?;
    let _ = ec.check_mem_magic();
    let _ = ec.read_memory(0x00, 0x0F);
    let _ = ec.get_temp_sensor_name(0);
    let _ = ec.get_charge_limit();
    let _ = ec.set_charge_limit(0, 100);
    let _ = ec.set_charge_current_limit(1000, Some(80));
    let _ = ec.fan_set_duty(None, 50);
    let _ = ec.autofanctrl(None);
    let _ = ec.version_info();
    let _ = ec.flash_version().map(|(_, _, c)| c == EcCurrentImage::RO);
    let _ = framework_lib::power::get_thermal(&ec).map(|t| (t.sensors.len(), t.fans.len()));
    let _ = framework_lib::smbios::get_product_name();
    let _ = framework_lib::smbios::get_baseboard_version().map(|v| format!("{:?}", v));
    let _ = framework_lib::smbios::get_smbios().and_then(|s| {
        s.structures().find_map(|r| match r {
            Ok(dmidecode::Structure::Bios(b)) => Some(b.bios_version.to_string()),
            _ => None,
        })
    });
    let res = EcRequestChargeStateGetV0 {
        cmd: ChargeStateCmd::GetState as u8,
        param: 0,
    }
    .send_command(&ec)
    .ok()?;
    let _ = { res.chg_voltage };
    let _ = framework_lib::built_info::PKG_VERSION;
    Some(())
}
