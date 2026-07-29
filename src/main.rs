//! Lockstep — milestone 1: enumerate render endpoints and report their IDs and
//! mix formats.

mod devices;

use anyhow::{Context, Result};
use devices::DeviceInfo;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

fn main() -> Result<()> {
    // COM apartment state is per-thread. Every audio thread added in later
    // milestones has to do this for itself; a single call here is not enough.
    //
    // SAFETY: paired with the CoUninitialize below, and nothing on this thread
    // touches COM before this point.
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        // S_FALSE means "already initialized on this thread" and is a success.
        // RPC_E_CHANGED_MODE means someone put this thread in an STA, which
        // would silently marshal every WASAPI call and is worth failing on.
        if hr.is_err() {
            if hr == RPC_E_CHANGED_MODE {
                anyhow::bail!(
                    "CoInitializeEx(COINIT_MULTITHREADED) failed: thread is already in a \
                     single-threaded apartment"
                );
            }
            return Err(windows::core::Error::from(hr))
                .context("CoInitializeEx(COINIT_MULTITHREADED) failed");
        }
    }

    let result = run();

    // SAFETY: balances the CoInitializeEx above; all COM interfaces obtained in
    // `run` have been dropped by now.
    unsafe { CoUninitialize() };

    result
}

fn run() -> Result<()> {
    let devices = devices::enumerate_render_endpoints()
        .context("failed to enumerate WASAPI render endpoints")?;

    println!("Lockstep — WASAPI render endpoints");
    println!("==================================");
    println!("{} endpoint(s) across all device states\n", devices.len());

    for device in &devices {
        print_device(device);
    }

    let active = devices.iter().filter(|d| d.state.is_active()).count();
    println!("{active} of {} endpoint(s) active", devices.len());

    Ok(())
}

fn print_device(device: &DeviceInfo) {
    let name = device
        .friendly_name
        .as_deref()
        .unwrap_or("<name unavailable>");
    println!("[{}] {}", device.index, name);

    // Printed verbatim: presets are keyed on this exact string, so it has to be
    // copy-pasteable out of this report.
    println!(
        "     {:<14}{}",
        "ID:",
        device.id.as_deref().unwrap_or("<unavailable>")
    );
    println!("     {:<14}{}", "State:", device.state.as_word());

    let mut roles: Vec<&str> = Vec::new();
    if device.is_default_console {
        roles.push("Console");
    }
    if device.is_default_multimedia {
        roles.push("Multimedia");
    }
    println!(
        "     {:<14}{}",
        "Default for:",
        if roles.is_empty() {
            "—".to_string()
        } else {
            roles.join(", ")
        }
    );

    match &device.mix_format {
        Some(format) => {
            println!(
                "     {:<14}{} Hz, {} ch, {}-bit",
                "Mix format:", format.sample_rate, format.channels, format.bits_per_sample
            );
            println!(
                "     {:<14}0x{:04X} ({})",
                "Format tag:",
                format.format_tag,
                format.container_name()
            );
            println!(
                "     {:<14}{} bytes/frame, {} bytes/sec, cbSize {}",
                "Framing:", format.block_align, format.avg_bytes_per_sec, format.cb_size
            );

            if let Some(ext) = &format.extensible {
                let speakers = ext.speaker_names();
                let speaker_list = if speakers.is_empty() {
                    "none (no positional bits set)".to_string()
                } else {
                    speakers.join(" ")
                };
                println!("     {:<14}{}", "Valid bits:", ext.valid_bits_per_sample);
                println!(
                    "     {:<14}0x{:08X} — {}",
                    "Channel mask:", ext.channel_mask, speaker_list
                );
                println!(
                    "     {:<14}{} ({:?})",
                    "SubFormat:",
                    ext.sub_format_name(),
                    ext.sub_format
                );
            }
        }
        None => {
            let reason = if device.state.is_active() {
                "<unavailable, see errors>"
            } else {
                "not queried (endpoint is not active)"
            };
            println!("     {:<14}{reason}", "Mix format:");
        }
    }

    for error in &device.errors {
        println!("     {:<14}{}", "ERROR:", error);
    }

    println!();
}
