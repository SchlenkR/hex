use std::ffi::CStr;
use std::process::Command;
use std::thread;

const SLOW_INFERENCE_BASE_MS: u128 = 500;
const SLOW_INFERENCE_MS_PER_AUDIO_SECOND: u128 = 50;

#[derive(Debug, Default, PartialEq, Eq)]
struct GpuUtilization {
    device: Option<u32>,
    renderer: Option<u32>,
    tiler: Option<u32>,
}

pub fn report_slow_inference(audio_ms: u64, inference_ms: u128) {
    if !is_slow(audio_ms, inference_ms) {
        return;
    }
    if let Err(error) = thread::Builder::new()
        .name("slow-inference-report".into())
        .spawn(move || {
            let gpu = gpu_utilization();
            tracing::warn!(
                audio_ms,
                inference_ms,
                gpu_device_pct = gpu.device,
                gpu_renderer_pct = gpu.renderer,
                gpu_tiler_pct = gpu.tiler,
                memory_pressure = memory_pressure(),
                "slow transcription inference"
            );
        })
    {
        tracing::warn!(%error, "could not report slow transcription inference");
    }
}

fn is_slow(audio_ms: u64, inference_ms: u128) -> bool {
    inference_ms
        > SLOW_INFERENCE_BASE_MS
            + u128::from(audio_ms) * SLOW_INFERENCE_MS_PER_AUDIO_SECOND / 1_000
}

fn gpu_utilization() -> GpuUtilization {
    Command::new("/usr/sbin/ioreg")
        .args(["-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"])
        .output()
        .ok()
        .map(|output| parse_gpu_utilization(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

fn parse_gpu_utilization(ioreg: &str) -> GpuUtilization {
    let value = |key: &str| {
        let pattern = format!("\"{key}\"=");
        let start = ioreg.find(&pattern)? + pattern.len();
        let digits = ioreg[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        digits.parse().ok()
    };
    GpuUtilization {
        device: value("Device Utilization %"),
        renderer: value("Renderer Utilization %"),
        tiler: value("Tiler Utilization %"),
    }
}

fn memory_pressure() -> &'static str {
    let name = c"kern.memorystatus_vm_pressure_level";
    let mut level: libc::c_int = 0;
    let mut size = size_of::<libc::c_int>();
    let status = unsafe {
        libc::sysctlbyname(
            CStr::as_ptr(name),
            (&raw mut level).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    match (status, level) {
        (0, 1) => "normal",
        (0, 2) => "warning",
        (0, 4) => "critical",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_threshold_scales_with_audio_length() {
        assert!(!is_slow(10_000, 900));
        assert!(is_slow(10_000, 1_100));
        assert!(!is_slow(120_000, 6_000));
        assert!(is_slow(1_000, 600));
    }

    #[test]
    fn gpu_utilization_is_read_from_performance_statistics() {
        let ioreg = r#"| "PerformanceStatistics" = {"Alloc system memory"=86410002432,"Tiler Utilization %"=27,"Renderer Utilization %"=45,"Device Utilization %"=50,"In use system memory"=8617705472}"#;

        assert_eq!(
            parse_gpu_utilization(ioreg),
            GpuUtilization {
                device: Some(50),
                renderer: Some(45),
                tiler: Some(27),
            }
        );
    }

    #[test]
    fn missing_statistics_yield_no_values() {
        assert_eq!(parse_gpu_utilization(""), GpuUtilization::default());
    }
}
