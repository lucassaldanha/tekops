//! Host resource facts, for `tekops doctor`.
//!
//! The parsing is pure and the I/O is a thin wrapper around it, the same split
//! `logfmt.rs` uses: `/proc` exists only on Linux (the node) and not on macOS
//! (the dev machine), so the interesting half has to be reachable from a test
//! that never touches a real file.

// The `doctor` command that calls into this module lands in a later task;
// until then nothing outside this file's own tests uses these items. Same
// pattern as `stack.rs` when it was first added ahead of its own wiring.
#![allow(dead_code)]

use std::path::Path;

const KIB: u64 = 1024;

/// `df -P` column count: Filesystem, blocks, Used, Available, Capacity, Mounted on.
const DF_MIN_FIELDS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Load {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Disk {
    pub available_bytes: u64,
    pub mount_point: String,
}

/// Reads one `Key: <value> kB` line out of `/proc/meminfo`.
fn meminfo_kib(s: &str, key: &str) -> Option<u64> {
    s.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_suffix("kB"))
        .and_then(|v| v.trim().trim_start_matches(':').trim().parse::<u64>().ok())
}

pub fn parse_meminfo(s: &str) -> Option<Memory> {
    Some(Memory {
        total_bytes: meminfo_kib(s, "MemTotal")? * KIB,
        // MemAvailable, never MemFree: the page cache keeps MemFree near zero
        // on a healthy box, so MemFree would alarm on every node.
        available_bytes: meminfo_kib(s, "MemAvailable")? * KIB,
    })
}

pub fn parse_loadavg(s: &str) -> Option<Load> {
    let mut f = s.split_whitespace();
    Some(Load {
        one: f.next()?.parse().ok()?,
        five: f.next()?.parse().ok()?,
        fifteen: f.next()?.parse().ok()?,
    })
}

/// Parses the single data line of `df -Pk <path>`.
///
/// Requires the POSIX one-line form. The default `df` format may wrap a long
/// device name onto its own line, which shifts every column; a short line is
/// therefore rejected rather than read, because misreporting free space is
/// worse than reporting none.
pub fn parse_df(s: &str) -> Option<Disk> {
    let line = s.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < DF_MIN_FIELDS {
        return None;
    }
    // The mount point is the last column and may contain spaces, so it is
    // rejoined from everything after the five fixed columns.
    let mount_point = fields[5..].join(" ");
    Some(Disk {
        available_bytes: fields[3].parse::<u64>().ok()? * KIB,
        mount_point,
    })
}

pub fn df_argv(path: &Path) -> Vec<String> {
    vec![
        "df".to_string(),
        "-Pk".to_string(),
        path.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_reads_total_and_available_in_bytes() {
        let s = "MemTotal:       32852796 kB\n\
                 MemFree:         1234567 kB\n\
                 MemAvailable:   28000000 kB\n";
        let got = parse_meminfo(s).unwrap();
        assert_eq!(got.total_bytes, 32_852_796 * 1024);
        assert_eq!(got.available_bytes, 28_000_000 * 1024);
    }

    /// MemAvailable, not MemFree. Free memory on a healthy Linux box is near
    /// zero because the page cache uses it, so reading MemFree would warn on
    /// every node.
    #[test]
    fn meminfo_is_none_without_memavailable() {
        let s = "MemTotal:       32852796 kB\nMemFree:  1234567 kB\n";
        assert!(parse_meminfo(s).is_none());
    }

    #[test]
    fn loadavg_reads_the_three_averages() {
        let got = parse_loadavg("1.20 0.90 0.80 2/1234 5678\n").unwrap();
        assert_eq!(got.one, 1.20);
        assert_eq!(got.five, 0.90);
        assert_eq!(got.fifteen, 0.80);
    }

    #[test]
    fn loadavg_is_none_when_truncated() {
        assert!(parse_loadavg("1.20 0.90\n").is_none());
    }

    #[test]
    fn df_reads_available_blocks_as_bytes_and_the_mount_point() {
        let s = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
                 /dev/sda1        961301832 517264000 395258448      57% /var/lib/docker\n";
        let got = parse_df(s).unwrap();
        assert_eq!(got.available_bytes, 395_258_448 * 1024);
        assert_eq!(got.mount_point, "/var/lib/docker");
    }

    /// A mount point may legally contain spaces, and it is the last column,
    /// so it must be rejoined rather than read as a single field.
    #[test]
    fn df_keeps_a_mount_point_containing_spaces() {
        let s = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                 /dev/sda1 100 50 50 50% /mnt/my disk\n";
        assert_eq!(parse_df(s).unwrap().mount_point, "/mnt/my disk");
    }

    /// `-P` is specified to put each filesystem on exactly one line. The
    /// default format may wrap a long device name onto a second line, which
    /// would make column 3 mean something else entirely. Misreading that is
    /// worse than reporting nothing, so the wrapped form must parse as None.
    #[test]
    fn df_rejects_the_wrapped_non_posix_form() {
        let s = "Filesystem           1024-blocks      Used Available Capacity Mounted on\n\
                 /dev/mapper/a-very-long-device-name\n\
                                        961301832 517264000 395258448      57% /\n";
        assert!(parse_df(s).is_none());
    }

    #[test]
    fn df_is_none_when_there_is_no_data_line() {
        let s = "Filesystem 1024-blocks Used Available Capacity Mounted on\n";
        assert!(parse_df(s).is_none());
    }

    #[test]
    fn df_argv_uses_the_posix_one_line_format_and_kibibyte_blocks() {
        assert_eq!(
            df_argv(Path::new("/var/lib/teku")),
            vec!["df", "-Pk", "/var/lib/teku"]
        );
    }
}
