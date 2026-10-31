//! Linux Kernel Buddy Allocator & Direct Compaction Scanner
//!
//! Interrogates `/proc/buddyinfo`, `/proc/vmstat`, and `/sys/kernel/mm/transparent_hugepage/`
//! to calculate external memory fragmentation, Order 9 (2MB HugePage) exhaustion, and direct compaction stalls.

use std::collections::HashMap;
use std::fs;

pub const PAGE_SIZE: u64 = 4096; // 4 KB standard page

#[derive(Debug, Clone)]
pub struct BuddyZone {
    pub node_id: u32,
    pub zone_name: String,
    pub free_blocks: [u64; 11], // Orders 0 to 10
    pub total_free_bytes: u64,
    pub order9_plus_bytes: u64,
    pub frag_index_order9: f64, // 0.0 = completely contiguous, 1.0 = completely fragmented
}

#[derive(Debug, Clone, Default)]
pub struct CompactionVmStats {
    pub compact_stall: u64,
    pub compact_fail: u64,
    pub compact_success: u64,
    pub compact_daemon_wake: u64,
    pub thp_fault_alloc: u64,
    pub thp_fault_fallback: u64,
    pub thp_collapse_alloc: u64,
    pub thp_collapse_alloc_failed: u64,
    pub thp_split_page: u64,
    pub allocstall_total: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ThpConfig {
    pub enabled: String,
    pub defrag: String,
}

#[derive(Debug, Clone, Default)]
pub struct BuddySnapshot {
    pub zones: Vec<BuddyZone>,
    pub vmstat: CompactionVmStats,
    pub thp_config: ThpConfig,
    pub global_free_bytes: u64,
    pub global_hugepage_free_bytes: u64,
    pub max_frag_index_order9: f64,
    pub is_compaction_stalling: bool,
    pub is_thp_exhausted: bool,
}

pub struct BuddyScanner;

impl BuddyScanner {
    /// Scans `/proc/buddyinfo` and `/proc/vmstat` to assess external memory fragmentation.
    pub fn scan() -> BuddySnapshot {
        let zones = Self::read_buddyinfo();
        let vmstat = Self::read_vmstat();
        let thp_config = Self::read_thp_config();

        let mut global_free_bytes = 0u64;
        let mut global_hugepage_free_bytes = 0u64;
        let mut max_frag_index = 0.0f64;

        for z in &zones {
            global_free_bytes = global_free_bytes.saturating_add(z.total_free_bytes);
            global_hugepage_free_bytes = global_hugepage_free_bytes.saturating_add(z.order9_plus_bytes);

            // Track worst fragmentation in active zones (ignore tiny DMA zone if unused)
            if z.zone_name != "DMA" && z.frag_index_order9 > max_frag_index {
                max_frag_index = z.frag_index_order9;
            }
        }

        // THP is exhausted if the Normal zone has zero Order 9/10 blocks or frag index > 0.85
        let is_thp_exhausted = max_frag_index >= 0.85;
        let is_compaction_stalling = vmstat.compact_stall > 0;

        BuddySnapshot {
            zones,
            vmstat,
            thp_config,
            global_free_bytes,
            global_hugepage_free_bytes,
            max_frag_index_order9: max_frag_index,
            is_compaction_stalling,
            is_thp_exhausted,
        }
    }

    fn read_buddyinfo() -> Vec<BuddyZone> {
        let mut zones = Vec::new();
        let content = fs::read_to_string("/proc/buddyinfo").unwrap_or_default();

        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Format: "Node 0, zone Normal 47 10 468 387 310 220 146 84 15 11 262"
            if parts.len() >= 15 && parts[0] == "Node" {
                let node_id = parts[1].trim_end_matches(',').parse::<u32>().unwrap_or(0);
                let zone_name = parts[3].to_string();

                let mut free_blocks = [0u64; 11];
                let mut total_free_bytes = 0u64;

                for order in 0..11 {
                    let count = parts[4 + order].parse::<u64>().unwrap_or(0);
                    free_blocks[order] = count;
                    let block_size = PAGE_SIZE << order;
                    total_free_bytes = total_free_bytes.saturating_add(count.saturating_mul(block_size));
                }

                // Order 9 (2MB) and Order 10 (4MB) bytes
                let order9_bytes = free_blocks[9].saturating_mul(PAGE_SIZE << 9);
                let order10_bytes = free_blocks[10].saturating_mul(PAGE_SIZE << 10);
                let order9_plus_bytes = order9_bytes.saturating_add(order10_bytes);

                let frag_index_order9 = if total_free_bytes > 0 {
                    let fragmented_bytes = total_free_bytes.saturating_sub(order9_plus_bytes);
                    fragmented_bytes as f64 / total_free_bytes as f64
                } else {
                    1.0
                };

                zones.push(BuddyZone {
                    node_id,
                    zone_name,
                    free_blocks,
                    total_free_bytes,
                    order9_plus_bytes,
                    frag_index_order9,
                });
            }
        }

        zones
    }

    fn read_vmstat() -> CompactionVmStats {
        let content = fs::read_to_string("/proc/vmstat").unwrap_or_default();
        let mut map = HashMap::new();

        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 2 {
                if let Ok(val) = parts[1].parse::<u64>() {
                    map.insert(parts[0], val);
                }
            }
        }

        let allocstalls = map.get("allocstall_normal").unwrap_or(&0)
            + map.get("allocstall_movable").unwrap_or(&0)
            + map.get("allocstall_dma32").unwrap_or(&0);

        CompactionVmStats {
            compact_stall: *map.get("compact_stall").unwrap_or(&0),
            compact_fail: *map.get("compact_fail").unwrap_or(&0),
            compact_success: *map.get("compact_success").unwrap_or(&0),
            compact_daemon_wake: *map.get("compact_daemon_wake").unwrap_or(&0),
            thp_fault_alloc: *map.get("thp_fault_alloc").unwrap_or(&0),
            thp_fault_fallback: *map.get("thp_fault_fallback").unwrap_or(&0),
            thp_collapse_alloc: *map.get("thp_collapse_alloc").unwrap_or(&0),
            thp_collapse_alloc_failed: *map.get("thp_collapse_alloc_failed").unwrap_or(&0),
            thp_split_page: *map.get("thp_split_page").unwrap_or(&0),
            allocstall_total: allocstalls,
        }
    }

    fn read_thp_config() -> ThpConfig {
        let enabled = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
            .unwrap_or_else(|_| "unknown".to_string())
            .trim()
            .to_string();

        let defrag = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/defrag")
            .unwrap_or_else(|_| "unknown".to_string())
            .trim()
            .to_string();

        ThpConfig { enabled, defrag }
    }
}
