//! Sound card helpers shared by live playback and the project screen.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// Largest block handed to a plugin in a single `process` call.
pub const MAX_BLOCK: usize = 1024;

/// Output devices of the default audio host, by name.
pub fn output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = match host.output_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => vec![],
    };
    names.dedup();
    names
}

/// The name of the system default output device, if any.
pub fn default_device_name() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}

pub(crate) fn find_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match name {
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device")),
        Some(name) => host
            .output_devices()
            .context("can't list output devices")?
            .find(|d| d.name().is_ok_and(|n| n == name))
            .ok_or_else(|| anyhow!("output device \"{name}\" not found")),
    }
}

/// The device's default configuration, or the best one at `rate`: stereo
/// first, then 32-bit float.
pub(crate) fn pick_config(
    device: &cpal::Device,
    rate: Option<u32>,
) -> Result<cpal::SupportedStreamConfig> {
    let Some(rate) = rate else {
        return Ok(device.default_output_config()?);
    };
    let mut candidates: Vec<_> = device
        .supported_output_configs()?
        .filter(|c| c.min_sample_rate().0 <= rate && rate <= c.max_sample_rate().0)
        .collect();
    candidates.sort_by_key(|c| {
        (
            c.channels() != 2,
            c.sample_format() != cpal::SampleFormat::F32,
            c.channels(),
        )
    });
    candidates
        .into_iter()
        .next()
        .map(|c| c.with_sample_rate(cpal::SampleRate(rate)))
        .ok_or_else(|| anyhow!("the device doesn't support {rate} Hz"))
}

/// Input devices of the default audio host, by name.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut names: Vec<String> = match host.input_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => vec![],
    };
    names.dedup();
    names
}

pub(crate) fn find_input_device(name: Option<&str>) -> Result<cpal::Device> {
    let host = cpal::default_host();
    match name {
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device")),
        Some(name) => host
            .input_devices()
            .context("can't list input devices")?
            .find(|d| d.name().is_ok_and(|n| n == name))
            .ok_or_else(|| anyhow!("input device \"{name}\" not found")),
    }
}
