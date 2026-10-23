// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Helpers shared by the integration tests and the in-crate browser tests (`src/tests.rs`
//! includes this file with `#[path]`): browser lookup, network canaries, PNG decoding and a
//! process scan.
#![allow(dead_code)]

use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A solid red 4×4 PNG.
pub const RED_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAQAAAAECAIAAAAmkwkpAAAAEElEQVR4nGP8z4AATAxEcQAz0QEHOoQ+uAAAAABJRU5ErkJggg==";

/// The browser to test with, or `None` (after printing why) when browser tests are skipped.
pub fn chrome() -> Option<PathBuf> {
    match std::env::var_os("CHROME_HEADLESS_SHELL") {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            assert!(
                std::env::var_os("CI").is_none(),
                "CHROME_HEADLESS_SHELL must be set in CI; browser tests may not be skipped there"
            );
            eprintln!("skipping browser test: CHROME_HEADLESS_SHELL is not set");
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    /// Start time in clock ticks since boot; with `pid` it identifies a process uniquely.
    pub start_time: u64,
    pub state: char,
    pub cmdline: String,
    pub environ: Vec<String>,
}

/// Every non-zombie process this user can inspect.
pub fn running_processes() -> Vec<ProcessInfo> {
    let mut processes = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return processes;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 1..].split_ascii_whitespace().collect();
        if fields.len() < 20 {
            continue;
        }
        let state = fields[0].chars().next().unwrap_or('?');
        if state == 'Z' || state == 'X' {
            continue;
        }
        let ppid = fields[1].parse().unwrap_or(0);
        let pgrp = fields[2].parse().unwrap_or(0);
        // Field 22 of /proc/<pid>/stat; `fields` starts at field 3.
        let start_time = fields[19].parse().unwrap_or(0);
        let read_nul_list = |name: &str| -> Vec<String> {
            std::fs::read(entry.path().join(name))
                .map(|bytes| {
                    bytes
                        .split(|&byte| byte == 0)
                        .filter(|part| !part.is_empty())
                        .map(|part| String::from_utf8_lossy(part).into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let cmdline = read_nul_list("cmdline").join(" ");
        let environ = read_nul_list("environ");
        processes.push(ProcessInfo {
            pid,
            ppid,
            pgrp,
            start_time,
            state,
            cmdline,
            environ,
        });
    }
    processes
}

/// Running processes started from the `chrome` executable. Chromium rewrites its children's
/// process titles, but `argv[0]` stays the executable path.
pub fn chrome_processes(chrome: &std::path::Path) -> Vec<ProcessInfo> {
    let prefix = chrome.display().to_string();
    running_processes()
        .into_iter()
        .filter(|process| process.cmdline.starts_with(&prefix))
        .collect()
}

/// `root` and every running process below it in the parent tree.
pub fn process_tree(root: u32) -> Vec<ProcessInfo> {
    let all = running_processes();
    let mut tree: Vec<ProcessInfo> = all
        .iter()
        .filter(|process| process.pid == root)
        .cloned()
        .collect();
    let mut index = 0;
    while index < tree.len() {
        let parent = tree[index].pid;
        tree.extend(all.iter().filter(|process| process.ppid == parent).cloned());
        index += 1;
    }
    tree
}

/// Decode a PNG into `(width, height, rgba)`.
pub fn decode_png(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = decoder.read_info().unwrap();
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buffer).unwrap();
    buffer.truncate(info.buffer_size());
    assert_eq!(
        info.color_type,
        png::ColorType::Rgba,
        "expected RGBA after normalization"
    );
    (info.width, info.height, buffer)
}

pub fn pixel(image: &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 4] {
    let index = ((y * image.0 + x) * 4) as usize;
    image.2[index..index + 4].try_into().unwrap()
}

/// TCP and UDP listeners on all interfaces that count every connection and datagram.
pub struct Canary {
    pub tcp_port: u16,
    pub udp_port: u16,
    tcp_hits: Arc<AtomicUsize>,
    udp_hits: Arc<AtomicUsize>,
    pub log: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Canary {
    pub fn start() -> Self {
        let tcp = TcpListener::bind("0.0.0.0:0").unwrap();
        let udp = UdpSocket::bind("0.0.0.0:0").unwrap();
        let tcp_port = tcp.local_addr().unwrap().port();
        let udp_port = udp.local_addr().unwrap().port();
        let tcp_hits = Arc::new(AtomicUsize::new(0));
        let udp_hits = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let hits = Arc::clone(&tcp_hits);
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                for stream in tcp.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    hits.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                    let mut head = [0u8; 256];
                    let read = std::io::Read::read(&mut stream, &mut head).unwrap_or(0);
                    let first_line = String::from_utf8_lossy(&head[..read])
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    log.lock().unwrap().push(format!("tcp {first_line}"));
                }
            });
        }
        {
            let hits = Arc::clone(&udp_hits);
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                let mut buffer = [0u8; 2048];
                while let Ok((length, from)) = udp.recv_from(&mut buffer) {
                    hits.fetch_add(1, Ordering::SeqCst);
                    log.lock()
                        .unwrap()
                        .push(format!("udp {length} bytes from {from}"));
                }
            });
        }
        Self {
            tcp_port,
            udp_port,
            tcp_hits,
            udp_hits,
            log,
        }
    }

    pub fn hits(&self) -> (usize, usize) {
        (
            self.tcp_hits.load(Ordering::SeqCst),
            self.udp_hits.load(Ordering::SeqCst),
        )
    }
}

/// Addresses of this host to aim the canary vectors at: loopback, plus the primary non-loopback
/// IPv4 address if there is one (found by "connecting" a UDP socket, which sends nothing). The
/// second address covers IP-literal URLs that never touch the host resolver.
pub fn canary_hosts() -> Vec<IpAddr> {
    let mut hosts = vec![IpAddr::from([127, 0, 0, 1])];
    let probe = UdpSocket::bind("0.0.0.0:0").and_then(|socket| {
        socket.connect(SocketAddr::from(([192, 0, 2, 1], 9)))?;
        socket.local_addr()
    });
    if let Ok(address) = probe
        && !address.ip().is_loopback()
        && !address.ip().is_unspecified()
    {
        hosts.push(address.ip());
    }
    hosts
}
