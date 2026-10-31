//! Linux Kernel Buddy Allocator External Memory Fragmentation & Compaction Sentinel
//!
//! Interrogates `/proc/buddyinfo`, `/proc/vmstat`, and `/sys/kernel/mm/transparent_hugepage/`
//! to calculate external memory fragmentation, Order 9 (2MB HugePage) exhaustion, and direct compaction stalls.

mod buddy_scanner;
mod exporter;

use buddy_scanner::{BuddyScanner, PAGE_SIZE};
use exporter::BuddyExporter;

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

fn print_usage(prog: &str) {
    println!(
        r#"{} - Linux Kernel Buddy Allocator External Fragmentation & Compaction Sentinel

USAGE:
    {} [OPTIONS]

OPTIONS:
    --port <PORT>        Prometheus HTTP exporter port (default: 9115)
    --interval <SECS>    Terminal refresh interval in seconds (default: 2.0)
    --daemon             Run in headless daemon mode (no terminal TUI)
    -h, --help           Show this help message

PROMETHEUS ENDPOINT:
    http://0.0.0.0:9115/metrics
"#,
        prog, prog
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut port: u16 = 9115;
    let mut interval_secs: f64 = 2.0;
    let mut daemon_mode = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                if i + 1 < args.len() {
                    port = args[i + 1].parse().unwrap_or(9115);
                    i += 1;
                }
            }
            "--interval" => {
                if i + 1 < args.len() {
                    interval_secs = args[i + 1].parse().unwrap_or(2.0);
                    i += 1;
                }
            }
            "--daemon" => {
                daemon_mode = true;
            }
            "-h" | "--help" => {
                print_usage(&args[0]);
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    // Perform initial scan
    let initial_snap = BuddyScanner::scan();
    let snapshot = Arc::new(RwLock::new(initial_snap.clone()));

    // Start Prometheus Exporter
    BuddyExporter::start_exporter(snapshot.clone(), port);

    println!(
        "[buddy-compact-sentinel] Initialized on 0.0.0.0:{}/metrics (PID: {})",
        port,
        std::process::id()
    );

    let baseline_vmstat = initial_snap.vmstat.clone();

    while running.load(Ordering::SeqCst) {
        let snap = BuddyScanner::scan();
        {
            let mut w = snapshot.write().unwrap();
            *w = snap.clone();
        }

        if !daemon_mode {
            // Clear screen
            print!("\x1B[2J\x1B[1;1H");
            println!("===============================================================================");
            println!("   BUDDY-COMPACT-SENTINEL: Linux Kernel Buddy Allocator & Compaction Auditor   ");
            println!("===============================================================================");
            println!(
                " Global Free RAM: {:>10}  |  Contiguous 2MB+ Free: {:>10}  |  PID: {}",
                format_bytes(snap.global_free_bytes),
                format_bytes(snap.global_hugepage_free_bytes),
                std::process::id()
            );

            let frag_pct = snap.max_frag_index_order9 * 100.0;
            let (status_label, status_code) = if frag_pct >= 85.0 {
                ("CRITICAL FRAGMENTATION", "\x1B[1;31m") // Red
            } else if frag_pct >= 60.0 {
                ("MODERATE FRAGMENTATION", "\x1B[1;33m") // Yellow
            } else {
                ("OPTIMAL CONTIGUITY", "\x1B[1;32m")     // Green
            };

            let stall_status = if snap.vmstat.compact_stall > baseline_vmstat.compact_stall {
                "\x1B[1;31m[CRITICAL: DIRECT COMPACTION STALLS DETECTED (SEV-0 LATENCY SPIKE)]\x1B[0m"
            } else if snap.vmstat.compact_stall > 0 {
                "\x1B[1;33m[ELEVATED: DIRECT COMPACTION STALLS RECORDED SINCE BOOT]\x1B[0m"
            } else {
                "\x1B[1;32m[NOMINAL: ZERO DIRECT COMPACTION STALLS DETECTED]\x1B[0m"
            };

            println!(
                " Order 9 (2MB THP) Frag Index: {}{:>5.1}%\x1B[0m [{}{}\x1B[0m]  |  THP: enabled=[{}] defrag=[{}]",
                status_code,
                frag_pct,
                status_code,
                status_label,
                snap.thp_config.enabled,
                snap.thp_config.defrag
            );
            println!(" Compaction Stall Status: {}", stall_status);
            println!("-------------------------------------------------------------------------------");

            println!(" BUDDY ALLOCATOR 11-ORDER CONTIGUITY PYRAMID (Zone Normal / DMA32)");
            println!(" ORDER | BLOCK SIZE | FREE BLOCKS | TOTAL MEMORY | CONTIGUOUS DISTRIBUTION");
            println!("-------+------------+-------------+--------------+------------------------");

            // Aggregate blocks across active zones for the pyramid
            let mut total_blocks_per_order = [0u64; 11];
            for zone in &snap.zones {
                for order in 0..11 {
                    total_blocks_per_order[order] = total_blocks_per_order[order].saturating_add(zone.free_blocks[order]);
                }
            }

            let max_blocks = total_blocks_per_order.iter().cloned().max().unwrap_or(1).max(1);

            for order in 0..11 {
                let block_bytes = PAGE_SIZE << order;
                let count = total_blocks_per_order[order];
                let order_bytes = count.saturating_mul(block_bytes);
                let bar_len = ((count as f64 / max_blocks as f64) * 24.0).round() as usize;
                let bar: String = "█".repeat(bar_len);

                let order_desc = match order {
                    0 => "4 KiB (Base)",
                    1 => "8 KiB",
                    2 => "16 KiB",
                    3 => "32 KiB",
                    4 => "64 KiB",
                    5 => "128 KiB",
                    6 => "256 KiB",
                    7 => "512 KiB",
                    8 => "1024 KiB (1M)",
                    9 => "2048 KiB (THP)",
                    10 => "4096 KiB (4M)",
                    _ => "",
                };

                let color = if order >= 9 {
                    "\x1B[1;36m" // Cyan for HugePages
                } else if order >= 6 {
                    "\x1B[0;32m" // Green for mid-range
                } else {
                    "\x1B[0;37m" // Light gray for small fragments
                };

                println!(
                    " {:>5} | {:<10} | {:>11} | {:>12} | {}{:<24}\x1B[0m",
                    order,
                    order_desc,
                    count,
                    format_bytes(order_bytes),
                    color,
                    bar
                );
            }

            println!("-------------------------------------------------------------------------------");
            println!(" DIRECT COMPACTION & THP LATENCY METRICS (Cumulative / Δ Delta)");
            println!(
                " compact_stall:         {:>10} (+{})  | thp_fault_alloc:           {:>10} (+{})",
                snap.vmstat.compact_stall,
                snap.vmstat.compact_stall.saturating_sub(baseline_vmstat.compact_stall),
                snap.vmstat.thp_fault_alloc,
                snap.vmstat.thp_fault_alloc.saturating_sub(baseline_vmstat.thp_fault_alloc)
            );
            println!(
                " compact_fail:          {:>10} (+{})  | thp_fault_fallback:        {:>10} (+{})",
                snap.vmstat.compact_fail,
                snap.vmstat.compact_fail.saturating_sub(baseline_vmstat.compact_fail),
                snap.vmstat.thp_fault_fallback,
                snap.vmstat.thp_fault_fallback.saturating_sub(baseline_vmstat.thp_fault_fallback)
            );
            println!(
                " compact_success:       {:>10} (+{})  | thp_collapse_alloc:        {:>10} (+{})",
                snap.vmstat.compact_success,
                snap.vmstat.compact_success.saturating_sub(baseline_vmstat.compact_success),
                snap.vmstat.thp_collapse_alloc,
                snap.vmstat.thp_collapse_alloc.saturating_sub(baseline_vmstat.thp_collapse_alloc)
            );
            println!(
                " compact_daemon_wake:   {:>10} (+{})  | allocstall_total:          {:>10} (+{})",
                snap.vmstat.compact_daemon_wake,
                snap.vmstat.compact_daemon_wake.saturating_sub(baseline_vmstat.compact_daemon_wake),
                snap.vmstat.allocstall_total,
                snap.vmstat.allocstall_total.saturating_sub(baseline_vmstat.allocstall_total)
            );
            println!("===============================================================================");
            println!(
                " Telemetry: http://0.0.0.0:{}/metrics  | Refresh: {:.1}s  | Press Ctrl+C to exit",
                port, interval_secs
            );
        }

        thread::sleep(Duration::from_secs_f64(interval_secs));
    }

    println!("\n[buddy-compact-sentinel] Shutting down gracefully...");
}
