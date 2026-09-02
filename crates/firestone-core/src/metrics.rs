//! One-shot machine metrics: the shared payload, Cloud Hypervisor counter
//! projection, and the Linux `/proc` sampling sources.
//!
//! Firestone runs no metrics daemon and stores no time series. Every counter
//! here is cumulative since the VMM process started, so clients derive rates
//! from two samples.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Cloud Hypervisor v53 counter key for bytes read from a block device.
const COUNTER_READ_BYTES: &str = "read_bytes";
/// Cloud Hypervisor v53 counter key for bytes written to a block device.
const COUNTER_WRITE_BYTES: &str = "write_bytes";
/// Cloud Hypervisor v53 counter key for completed block reads.
const COUNTER_READ_OPS: &str = "read_ops";
/// Cloud Hypervisor v53 counter key for completed block writes.
const COUNTER_WRITE_OPS: &str = "write_ops";

/// Nanoseconds in one second.
const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;
/// Bytes in one kibibyte, the unit `/proc/<pid>/status` reports.
const BYTES_PER_KIB: u64 = 1024;

/// Lowest counter value Firestone treats as an unavailable sentinel.
///
/// Cloud Hypervisor v53 reports an unexercised latency counter as `u64::MAX`
/// for min and max, and an average derived from those saturating values. No
/// real byte, operation, or packet count reaches 2^63, so any counter at or
/// above this floor is projected as absent instead of being surfaced.
pub const COUNTER_SENTINEL_FLOOR: u64 = 1 << 63;

/// Reports whether one raw VMM counter value is an unavailable sentinel.
#[must_use]
pub const fn counter_is_sentinel(value: u64) -> bool {
    value >= COUNTER_SENTINEL_FLOOR
}

/// One cumulative resource sample for a running machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsResult {
    pub sampled_at: String,
    pub cpu: MetricsCpu,
    pub memory: MetricsMemory,
    pub block: Vec<MetricsBlockDevice>,
    pub net: Option<Vec<MetricsNetDevice>>,
}

/// Guest vCPU count and cumulative VMM processor time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsCpu {
    pub vcpus: u8,
    pub cpu_time_ns: Option<u64>,
}

/// Host and guest memory figures for one sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsMemory {
    pub rss_bytes: Option<u64>,
    pub allocated_bytes: u64,
    pub guest_actual_bytes: Option<u64>,
}

/// Cumulative virtio-block counters for one guest disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsBlockDevice {
    pub device: String,
    pub read_bytes: Option<u64>,
    pub written_bytes: Option<u64>,
    pub read_ops: Option<u64>,
    pub write_ops: Option<u64>,
}

/// Cumulative counters for one guest network device, named as the VMM names
/// them because Firestone's default `passt` vhost-user path reports none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsNetDevice {
    pub device: String,
    pub counters: BTreeMap<String, u64>,
}

/// One on-demand sample of the host Firestone runs on.
///
/// The host sample follows the same rules as the machine sample: it is read
/// only, it stores nothing, and an unavailable figure is `null` rather than a
/// zero. Unlike the machine sample it is levels, not counters, so one reading
/// stands on its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostMetricsResult {
    pub sampled_at: String,
    pub cpu: HostMetricsCpu,
    pub memory: HostMetricsMemory,
    pub disk: HostMetricsDisk,
}

/// Host processor count and kernel load averages.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HostMetricsCpu {
    pub cores: Option<u32>,
    pub load_average_1m: Option<f64>,
    pub load_average_5m: Option<f64>,
    pub load_average_15m: Option<f64>,
}

/// Host memory levels, as the kernel reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetricsMemory {
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
}

/// Capacity of the filesystem holding the Firestone data directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostMetricsDisk {
    pub path: String,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
}

/// Reads `MemTotal` and `MemAvailable` from one `/proc/meminfo` document.
///
/// Both are reported in kibibytes and returned as bytes. A missing or
/// malformed line is absent, never zero.
#[must_use]
pub fn parse_proc_meminfo_bytes(contents: &str) -> (Option<u64>, Option<u64>) {
    (
        meminfo_field(contents, "MemTotal:"),
        meminfo_field(contents, "MemAvailable:"),
    )
}

fn meminfo_field(contents: &str, name: &str) -> Option<u64> {
    let value = contents
        .lines()
        .find_map(|line| line.strip_prefix(name))?
        .trim();
    let kibibytes = value.strip_suffix("kB")?.trim_end().parse::<u64>().ok()?;
    kibibytes.checked_mul(BYTES_PER_KIB)
}

/// Reads the 1, 5, and 15 minute load averages from one `/proc/loadavg` line.
#[must_use]
pub fn parse_proc_loadavg(contents: &str) -> Option<[f64; 3]> {
    let mut fields = contents.split_ascii_whitespace();
    let mut next = || {
        fields
            .next()
            .and_then(|field| field.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0)
    };
    Some([next()?, next()?, next()?])
}

/// Total and available bytes on the filesystem holding `path`.
///
/// Available counts the blocks this user may still use, which is what a
/// reader deciding whether another image fits actually needs. A path that
/// does not exist yet is measured through its nearest existing ancestor: that
/// is the filesystem it will be created on.
#[must_use]
pub fn filesystem_capacity(path: &std::path::Path) -> Option<(u64, u64)> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        if let Ok(stats) = nix::sys::statvfs::statvfs(current) {
            // statvfs field widths differ between the supported targets, so
            // every count is widened through one generic conversion rather
            // than a per-target cast.
            let fragment = widen(stats.fragment_size());
            return Some((
                fragment.saturating_mul(widen(stats.blocks())),
                fragment.saturating_mul(widen(stats.blocks_available())),
            ));
        }
        candidate = current.parent();
    }
    None
}

fn widen<T: Into<u64>>(value: T) -> u64 {
    value.into()
}

/// Samples host processor and memory levels.
///
/// `/proc` is Linux only, so a host without it reports absent memory and load
/// figures rather than failing. The core count comes from the standard library
/// and is available everywhere.
#[must_use]
pub fn sample_host(
    data_dir: &std::path::Path,
) -> (HostMetricsCpu, HostMetricsMemory, HostMetricsDisk) {
    let cores = std::thread::available_parallelism()
        .ok()
        .and_then(|count| u32::try_from(count.get()).ok());
    let load = read_proc("/proc/loadavg")
        .as_deref()
        .and_then(parse_proc_loadavg);
    let (total_bytes, available_bytes) = read_proc("/proc/meminfo")
        .as_deref()
        .map_or((None, None), parse_proc_meminfo_bytes);
    let capacity = filesystem_capacity(data_dir);
    (
        HostMetricsCpu {
            cores,
            load_average_1m: load.map(|load| load[0]),
            load_average_5m: load.map(|load| load[1]),
            load_average_15m: load.map(|load| load[2]),
        },
        HostMetricsMemory {
            total_bytes,
            available_bytes,
        },
        HostMetricsDisk {
            path: data_dir.display().to_string(),
            total_bytes: capacity.map(|capacity| capacity.0),
            available_bytes: capacity.map(|capacity| capacity.1),
        },
    )
}

fn read_proc(path: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(path).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

/// Host process figures sampled from `/proc` for the machine's VMM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VmmProcessSample {
    pub cpu_time_ns: Option<u64>,
    pub rss_bytes: Option<u64>,
}

/// Splits one `vm.counters` map into the block and network projections.
///
/// A device is a block device when it reports both byte counters. Sentinel
/// values are dropped everywhere, so an unexercised counter is `null` rather
/// than a saturating number. Network devices are `None` when the VMM reports
/// none at all, which is the verified v53 behavior under vhost-user `passt`.
#[must_use]
pub fn project_device_counters(
    counters: &BTreeMap<String, BTreeMap<String, u64>>,
) -> (Vec<MetricsBlockDevice>, Option<Vec<MetricsNetDevice>>) {
    let mut block = Vec::new();
    let mut net = Vec::new();
    for (device, values) in counters {
        if values.contains_key(COUNTER_READ_BYTES) && values.contains_key(COUNTER_WRITE_BYTES) {
            block.push(MetricsBlockDevice {
                device: device.clone(),
                read_bytes: reported_counter(values, COUNTER_READ_BYTES),
                written_bytes: reported_counter(values, COUNTER_WRITE_BYTES),
                read_ops: reported_counter(values, COUNTER_READ_OPS),
                write_ops: reported_counter(values, COUNTER_WRITE_OPS),
            });
        } else {
            net.push(MetricsNetDevice {
                device: device.clone(),
                counters: values
                    .iter()
                    .filter(|(_, value)| !counter_is_sentinel(**value))
                    .map(|(key, value)| (key.clone(), *value))
                    .collect(),
            });
        }
    }
    (block, (!net.is_empty()).then_some(net))
}

fn reported_counter(values: &BTreeMap<String, u64>, key: &str) -> Option<u64> {
    values
        .get(key)
        .copied()
        .filter(|value| !counter_is_sentinel(*value))
}

/// Converts scheduler ticks to nanoseconds for one clock-tick frequency.
#[must_use]
pub fn cpu_ticks_to_nanoseconds(ticks: u64, ticks_per_second: u64) -> Option<u64> {
    if ticks_per_second == 0 {
        return None;
    }
    u64::try_from(u128::from(ticks) * NANOSECONDS_PER_SECOND / u128::from(ticks_per_second)).ok()
}

/// Sums `utime` and `stime` from one `/proc/<pid>/stat` line.
///
/// Fields 14 and 15 are read relative to the closing parenthesis of field 2,
/// because a process name may itself contain spaces and parentheses.
#[must_use]
pub fn parse_proc_stat_cpu_ticks(contents: &str) -> Option<u64> {
    let (_, tail) = contents.rsplit_once(')')?;
    let fields = tail.split_ascii_whitespace().collect::<Vec<_>>();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    utime.checked_add(stime)
}

/// Reads `VmRSS` from one `/proc/<pid>/status` document as bytes.
#[must_use]
pub fn parse_proc_status_rss_bytes(contents: &str) -> Option<u64> {
    let value = contents
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .trim();
    let kibibytes = value.strip_suffix("kB")?.trim_end().parse::<u64>().ok()?;
    kibibytes.checked_mul(BYTES_PER_KIB)
}

/// Samples the VMM process's cumulative CPU time and resident set size.
///
/// Sampling is best effort: a process that exited between the state read and
/// the sample yields `None` fields rather than failing the action. Non-Linux
/// hosts have no `/proc` contract and always report `None`.
#[must_use]
pub fn sample_vmm_process(pid: u32) -> VmmProcessSample {
    #[cfg(target_os = "linux")]
    {
        let cpu_time_ns = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .as_deref()
            .and_then(parse_proc_stat_cpu_ticks)
            .and_then(|ticks| {
                cpu_ticks_to_nanoseconds(ticks, rustix::param::clock_ticks_per_second())
            });
        let rss_bytes = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .as_deref()
            .and_then(parse_proc_status_rss_bytes);
        VmmProcessSample {
            cpu_time_ns,
            rss_bytes,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        VmmProcessSample::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        COUNTER_SENTINEL_FLOOR, MetricsBlockDevice, MetricsNetDevice, counter_is_sentinel,
        cpu_ticks_to_nanoseconds, parse_proc_stat_cpu_ticks, parse_proc_status_rss_bytes,
        project_device_counters, sample_vmm_process,
    };

    fn counters(entries: &[(&str, &[(&str, u64)])]) -> BTreeMap<String, BTreeMap<String, u64>> {
        entries
            .iter()
            .map(|(device, values)| {
                (
                    (*device).to_owned(),
                    values
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), *value))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn proc_stat_process_name_with_spaces_returns_utime_plus_stime() {
        let line = "41207 (cloud hyper(visor)) S 1 41207 41207 0 -1 4194560 5182 0 0 0 731 219 0 0 20 0 9 0 4364 0 0\n";
        assert_eq!(parse_proc_stat_cpu_ticks(line), Some(950));
    }

    #[test]
    fn proc_stat_truncated_or_non_numeric_fields_return_none() {
        for line in [
            "",
            "41207 (vmm) S 1 2 3",
            "41207 (vmm) S 1 41207 41207 0 -1 4194560 5182 0 0 0 x 219 0 0 20 0 9 0 4364",
            "41207 vmm S 1 41207 41207 0 -1 4194560 5182 0 0 0 731 219 0 0 20 0 9 0 4364",
        ] {
            assert_eq!(parse_proc_stat_cpu_ticks(line), None, "accepted {line:?}");
        }
    }

    #[test]
    fn proc_status_vmrss_kibibytes_return_bytes() {
        let document = "Name:\tcloud-hypervisor\nState:\tS (sleeping)\nVmPeak:\t 2148000 kB\nVmRSS:\t   65536 kB\nThreads:\t9\n";
        assert_eq!(parse_proc_status_rss_bytes(document), Some(67_108_864));
    }

    #[test]
    fn proc_status_missing_or_malformed_vmrss_returns_none() {
        for document in [
            "Name:\tcloud-hypervisor\n",
            "VmRSS:\t   65536\n",
            "VmRSS:\tmany kB\n",
            "VmRSS:\n",
        ] {
            assert_eq!(
                parse_proc_status_rss_bytes(document),
                None,
                "accepted {document:?}"
            );
        }
    }

    #[test]
    fn cpu_ticks_convert_with_clock_frequency_and_reject_zero() {
        assert_eq!(cpu_ticks_to_nanoseconds(950, 100), Some(9_500_000_000));
        assert_eq!(cpu_ticks_to_nanoseconds(0, 100), Some(0));
        assert_eq!(cpu_ticks_to_nanoseconds(1, 0), None);
    }

    #[test]
    fn counter_sentinel_floor_covers_saturating_latency_values() {
        assert!(counter_is_sentinel(u64::MAX));
        assert!(counter_is_sentinel(u64::MAX / 2 + 1));
        assert!(counter_is_sentinel(COUNTER_SENTINEL_FLOOR));
        assert!(!counter_is_sentinel(COUNTER_SENTINEL_FLOOR - 1));
        assert!(!counter_is_sentinel(0));
    }

    #[test]
    fn device_counters_drop_sentinels_and_classify_block_and_net() {
        let sample = counters(&[
            (
                "_disk0",
                &[
                    ("read_bytes", 4096),
                    ("write_bytes", 8192),
                    ("read_ops", 2),
                    ("write_ops", 3),
                    ("read_latency_min", 11),
                    ("write_latency_min", u64::MAX),
                    ("write_latency_max", u64::MAX),
                    ("write_latency_avg", u64::MAX / 2 + 7),
                ],
            ),
            (
                "_disk1",
                &[
                    ("read_bytes", 0),
                    ("write_bytes", 0),
                    ("read_ops", 0),
                    ("write_ops", u64::MAX),
                ],
            ),
            ("_net0", &[("rx_bytes", 128), ("tx_bytes", u64::MAX)]),
        ]);

        let (block, net) = project_device_counters(&sample);
        assert_eq!(
            block,
            vec![
                MetricsBlockDevice {
                    device: "_disk0".to_owned(),
                    read_bytes: Some(4096),
                    written_bytes: Some(8192),
                    read_ops: Some(2),
                    write_ops: Some(3),
                },
                MetricsBlockDevice {
                    device: "_disk1".to_owned(),
                    read_bytes: Some(0),
                    written_bytes: Some(0),
                    read_ops: Some(0),
                    write_ops: None,
                },
            ]
        );
        assert_eq!(
            net,
            Some(vec![MetricsNetDevice {
                device: "_net0".to_owned(),
                counters: BTreeMap::from([("rx_bytes".to_owned(), 128)]),
            }])
        );
    }

    #[test]
    fn device_counters_without_net_entries_return_none() {
        let sample = counters(&[(
            "_disk0",
            &[
                ("read_bytes", 1),
                ("write_bytes", 2),
                ("read_ops", 3),
                ("write_ops", 4),
            ],
        )]);
        let (block, net) = project_device_counters(&sample);
        assert_eq!(block.len(), 1);
        assert_eq!(net, None);
    }

    #[test]
    fn device_counters_empty_map_returns_no_devices() {
        let (block, net) = project_device_counters(&BTreeMap::new());
        assert!(block.is_empty());
        assert_eq!(net, None);
    }

    #[test]
    fn proc_meminfo_reports_total_and_available_in_bytes() {
        let document = "MemTotal:       16316196 kB\nMemFree:          204584 kB\nMemAvailable:    9218044 kB\nBuffers:           12345 kB\n";
        assert_eq!(
            super::parse_proc_meminfo_bytes(document),
            (Some(16_707_784_704), Some(9_439_277_056))
        );
    }

    #[test]
    fn proc_meminfo_missing_or_malformed_fields_are_absent() {
        for document in [
            "",
            "MemTotal:\n",
            "MemTotal:       16316196\n",
            "MemTotal:       many kB\n",
            "MemFree:          204584 kB\n",
        ] {
            assert_eq!(
                super::parse_proc_meminfo_bytes(document),
                (None, None),
                "accepted {document:?}"
            );
        }
    }

    #[test]
    fn proc_loadavg_reads_three_averages_and_rejects_malformed_lines() {
        assert_eq!(
            super::parse_proc_loadavg("0.45 0.30 0.25 1/234 5678\n"),
            Some([0.45, 0.30, 0.25])
        );
        for line in ["", "0.45 0.30", "x 0.30 0.25 1/2 3", "-1 0.30 0.25 1/2 3"] {
            assert_eq!(super::parse_proc_loadavg(line), None, "accepted {line:?}");
        }
    }

    #[test]
    fn filesystem_capacity_measures_a_directory_and_falls_back_to_its_parent()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let measured =
            super::filesystem_capacity(directory.path()).ok_or("no capacity for a temp dir")?;
        assert!(measured.0 > 0, "total {}", measured.0);
        assert!(
            measured.1 <= measured.0,
            "available {} of {}",
            measured.1,
            measured.0
        );
        // A data directory doctor has not created yet lives on the filesystem
        // its parent is on, which is the figure worth reporting.
        assert_eq!(
            super::filesystem_capacity(&directory.path().join("not/created/yet")),
            Some(measured)
        );
        Ok(())
    }

    #[test]
    fn host_sample_reports_the_measured_data_directory_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let (cpu, memory, disk) = super::sample_host(directory.path());
        assert_eq!(disk.path, directory.path().display().to_string());
        assert!(disk.total_bytes.is_some());
        assert!(cpu.cores.is_some_and(|cores| cores >= 1));
        if cfg!(target_os = "linux") {
            assert!(memory.total_bytes.is_some());
            assert!(cpu.load_average_1m.is_some());
        } else {
            assert_eq!(memory.total_bytes, None);
            assert_eq!(cpu.load_average_1m, None);
        }
        Ok(())
    }

    #[test]
    fn vmm_process_sample_for_unknown_pid_reports_absent_fields() {
        let sample = sample_vmm_process(u32::MAX);
        assert_eq!(sample.cpu_time_ns, None);
        assert_eq!(sample.rss_bytes, None);
    }
}
