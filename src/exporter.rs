//! Prometheus Metrics Exporter on :9115/metrics
//!
//! Exposes Linux Buddy Allocator external memory fragmentation indices,
//! Order 9 (2MB HugePage) exhaustion, and Direct Memory Compaction latency stalls.

use crate::buddy_scanner::BuddySnapshot;
use std::sync::{Arc, RwLock};
use std::thread;
use tiny_http::{Response, Server};

pub struct BuddyExporter;

impl BuddyExporter {
    pub fn start_exporter(snapshot: Arc<RwLock<BuddySnapshot>>, port: u16) {
        thread::spawn(move || {
            let addr = format!("0.0.0.0:{}", port);
            let server = match Server::http(&addr) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Prometheus HTTP server failed to bind to {}: {}", addr, e);
                    return;
                }
            };

            for request in server.incoming_requests() {
                if request.url() == "/metrics" || request.url() == "/" {
                    let snap = snapshot.read().unwrap().clone();

                    let mut body = String::with_capacity(8192);

                    body.push_str("# HELP buddy_global_free_bytes Total free memory managed by the Linux Buddy Allocator in bytes\n");
                    body.push_str("# TYPE buddy_global_free_bytes gauge\n");
                    body.push_str(&format!("buddy_global_free_bytes {}\n", snap.global_free_bytes));

                    body.push_str("# HELP buddy_global_hugepage_free_bytes Free memory available in contiguous 2MB+ blocks (Order 9 & 10)\n");
                    body.push_str("# TYPE buddy_global_hugepage_free_bytes gauge\n");
                    body.push_str(&format!("buddy_global_hugepage_free_bytes {}\n", snap.global_hugepage_free_bytes));

                    body.push_str("# HELP buddy_max_fragmentation_index_order9 Peak External Fragmentation Index for 2MB HugePages (0.0 to 1.0)\n");
                    body.push_str("# TYPE buddy_max_fragmentation_index_order9 gauge\n");
                    body.push_str(&format!("buddy_max_fragmentation_index_order9 {:.4}\n", snap.max_frag_index_order9));

                    body.push_str("# HELP buddy_thp_exhausted_active 1 if Order 9 2MB HugePages are severely fragmented (>85% index)\n");
                    body.push_str("# TYPE buddy_thp_exhausted_active gauge\n");
                    body.push_str(&format!("buddy_thp_exhausted_active {}\n", if snap.is_thp_exhausted { 1 } else { 0 }));

                    body.push_str("# HELP buddy_compaction_stalling_active 1 if Direct Memory Compaction stalls have occurred\n");
                    body.push_str("# TYPE buddy_compaction_stalling_active gauge\n");
                    body.push_str(&format!("buddy_compaction_stalling_active {}\n", if snap.is_compaction_stalling { 1 } else { 0 }));

                    // Compaction and THP counters
                    body.push_str("# HELP buddy_compact_stall_total Cumulative allocations that stalled in Direct Memory Compaction\n");
                    body.push_str("# TYPE buddy_compact_stall_total counter\n");
                    body.push_str(&format!("buddy_compact_stall_total {}\n", snap.vmstat.compact_stall));

                    body.push_str("# HELP buddy_compact_fail_total Cumulative Direct Compaction attempts that failed to find contiguous space\n");
                    body.push_str("# TYPE buddy_compact_fail_total counter\n");
                    body.push_str(&format!("buddy_compact_fail_total {}\n", snap.vmstat.compact_fail));

                    body.push_str("# HELP buddy_compact_success_total Cumulative Direct Compaction attempts that succeeded\n");
                    body.push_str("# TYPE buddy_compact_success_total counter\n");
                    body.push_str(&format!("buddy_compact_success_total {}\n", snap.vmstat.compact_success));

                    body.push_str("# HELP buddy_thp_fault_alloc_total Transparent HugePages successfully allocated on page fault\n");
                    body.push_str("# TYPE buddy_thp_fault_alloc_total counter\n");
                    body.push_str(&format!("buddy_thp_fault_alloc_total {}\n", snap.vmstat.thp_fault_alloc));

                    body.push_str("# HELP buddy_thp_fault_fallback_total Times THP allocation failed due to fragmentation and fell back to 4KB\n");
                    body.push_str("# TYPE buddy_thp_fault_fallback_total counter\n");
                    body.push_str(&format!("buddy_thp_fault_fallback_total {}\n", snap.vmstat.thp_fault_fallback));

                    body.push_str("# HELP buddy_thp_collapse_alloc_failed_total Failed khugepaged attempts to collapse 4KB pages into 2MB HugePage\n");
                    body.push_str("# TYPE buddy_thp_collapse_alloc_failed_total counter\n");
                    body.push_str(&format!("buddy_thp_collapse_alloc_failed_total {}\n", snap.vmstat.thp_collapse_alloc_failed));

                    body.push_str("# HELP buddy_thp_split_page_total Transparent HugePages split into individual 4KB pages\n");
                    body.push_str("# TYPE buddy_thp_split_page_total counter\n");
                    body.push_str(&format!("buddy_thp_split_page_total {}\n", snap.vmstat.thp_split_page));

                    body.push_str("# HELP buddy_allocstall_total Direct page allocation stalls across all memory zones\n");
                    body.push_str("# TYPE buddy_allocstall_total counter\n");
                    body.push_str(&format!("buddy_allocstall_total {}\n", snap.vmstat.allocstall_total));

                    // Per-zone metrics
                    for z in &snap.zones {
                        body.push_str(&format!(
                            "buddy_zone_free_bytes{{node=\"{}\",zone=\"{}\"}} {}\n",
                            z.node_id, z.zone_name, z.total_free_bytes
                        ));
                        body.push_str(&format!(
                            "buddy_zone_hugepage_free_bytes{{node=\"{}\",zone=\"{}\"}} {}\n",
                            z.node_id, z.zone_name, z.order9_plus_bytes
                        ));
                        body.push_str(&format!(
                            "buddy_zone_fragmentation_index{{node=\"{}\",zone=\"{}\",order=\"9\"}} {:.4}\n",
                            z.node_id, z.zone_name, z.frag_index_order9
                        ));

                        for order in 0..11 {
                            body.push_str(&format!(
                                "buddy_zone_free_blocks{{node=\"{}\",zone=\"{}\",order=\"{}\"}} {}\n",
                                z.node_id, z.zone_name, order, z.free_blocks[order]
                            ));
                        }
                    }

                    let response = Response::from_string(body)
                        .with_header(tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; version=0.0.4"[..]).unwrap());
                    let _ = request.respond(response);
                } else {
                    let response = Response::from_string("Not Found").with_status_code(404);
                    let _ = request.respond(response);
                }
            }
        });
    }
}
