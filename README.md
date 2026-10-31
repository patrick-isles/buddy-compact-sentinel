# buddy-compact-sentinel

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Telemetry](https://img.shields.io/badge/Prometheus-Port_9115-blueviolet.svg)](#prometheus-telemetry-endpoint)
[![Kernel MM](https://img.shields.io/badge/Kernel-Buddy_Allocator_&_THP_Compaction-red.svg)](https://www.kernel.org/doc/gorman/html/understand/understand009.html)

**Linux Kernel Buddy Allocator External Memory Fragmentation & Direct Compaction Stall Sentinel in pure Rust.**

`buddy-compact-sentinel` continuously interrogates the Linux page allocator (`/proc/buddyinfo`), virtual memory statistics (`/proc/vmstat`), and Transparent HugePage policies (`/sys/kernel/mm/transparent_hugepage/`) to calculate mathematical external fragmentation indices, detect Order 9 (2MB HugePage) exhaustion, and alert on Sev-0 **Direct Memory Compaction stalls** (`compact_stall`).

---

## The Sev-0 Problem: The Compaction Latency Cliff

High-throughput datastores, model servers, and low-latency distributed systems (PostgreSQL, Redis, RocksDB, ScyllaDB, vLLM, Triton) rely on 2MB Transparent HugePages (THP) to minimize translation lookaside buffer (TLB) misses.

However, as a Linux server runs over days or weeks under sustained I/O and memory churn, the physical address space undergoes **External Memory Fragmentation**:
1. Free memory remains abundant in aggregate (e.g., 20 GiB free).
2. But free memory is shattered into millions of isolated 4 KiB pages (Order 0). Contiguous blocks of Order 9 (2 MiB / 512 pages) fall to zero.
3. When an application thread requests or faults on a 2 MiB page and no contiguous block exists, the kernel triggers **Direct Memory Compaction** (`compact_stall`).
4. **The Latency Trap:** The requesting user thread is pulled off its task and forced to execute synchronous page compaction inside the kernel. The kernel scans pageframes, allocates target pages, copies 4 KiB memory pages across NUMA zones, and rewrites page tables while holding zone locks.
5. P99/P999 query latency instantly spikes from 200 µs to **500 ms+**, cascading timeouts across distributed microservices.

```
+-----------------------------------------------------------------------------+
|                      THE 500ms DIRECT COMPACTION CLIFF                      |
+-----------------------------------------------------------------------------+
 Application Thread: malloc(2MB) / THP Page Fault
        │
        ▼
 Kernel Buddy Allocator: Checks Order 9 (2MB blocks) in Zone Normal
        │
        ├─► [Contiguous Order 9 Available] ────────► Fast Path (100 ns)
        │
        └─► [Order 9 Fragmented / Exhausted]
                   │
                   ▼
       DIRECT MEMORY COMPACTION (`compact_stall`)
        ├─ Application thread suspended in kernel space
        ├─ Scans memory zone for free vs. unmovable pages
        ├─ Synchronously migrates hundreds of 4KB pages
        └─ P99 Latency Spikes: 200 µs ──► 500 ms (Sev-0 Cascade)
```

`buddy-compact-sentinel` alerts systems engineers **before** compaction stalls occur by quantifying Order 9 fragmentation decay in real time.

---

## Buddy Allocator 11-Order Contiguity Pyramid

The Linux Buddy Allocator manages physical memory in power-of-two blocks from Order 0 (4 KiB) up to Order 10 (4 MiB):

| Order | Page Count | Block Size | Typical Allocation Target |
|:-----:|:----------:|:----------:|:--------------------------|
| **0** | 1 page | 4 KiB | Base userspace page allocations |
| **1** | 2 pages | 8 KiB | Small kernel structures, socket buffers |
| **2** | 4 pages | 16 KiB | `kmalloc-16k`, pipe buffers |
| **3** | 8 pages | 32 KiB | Network drivers, slab allocation |
| **4** | 16 pages | 64 KiB | High-order kernel buffers |
| **5** | 32 pages | 128 KiB | Driver DMA descriptors |
| **6** | 64 pages | 256 KiB | Large skb buffers, slab caches |
| **7** | 128 pages | 512 KiB | High-bandwidth network rings |
| **8** | 256 pages | 1024 KiB (1 MiB) | Large contiguous kernel buffers |
| **9** | **512 pages** | **2048 KiB (2 MiB)** | **Transparent HugePages (THP), KVM HugePages** |
| **10**| **1024 pages** | **4096 KiB (4 MiB)** | **Maximum contiguous order in standard Linux x86** |

---

## Mathematical External Fragmentation Index

For any memory zone $Z$ and target page order $k$ (where $k=9$ represents 2 MiB HugePages), the External Fragmentation Index $R_{\text{frag}}(k)$ is computed as:

$$R_{\text{frag}}(k) = \frac{\sum_{i=0}^{k-1} 2^i \cdot \text{blocks}[i]}{\sum_{i=0}^{10} 2^i \cdot \text{blocks}[i]} = \frac{\text{TotalFreeBytes} - \text{Order}_{\ge k}\text{Bytes}}{\text{TotalFreeBytes}}$$

- **$R_{\text{frag}} = 0.0$**: Pristine contiguous memory. 100% of free memory resides in blocks of Order 9 or higher. Zero risk of compaction stalls.
- **$R_{\text{frag}} \in [0.60, 0.84]$**: Moderate fragmentation. Order 9 blocks are depleting; background kcompactd activity increases.
- **$R_{\text{frag}} \ge 0.85$**: Severe external fragmentation. Order 9 free blocks are nearly exhausted. High risk of synchronous Direct Compaction latency spikes.

---

## Real-Time Terminal TUI

When run interactively, `buddy-compact-sentinel` renders the live state of the kernel buddy allocator and compaction subsystem:

```
===============================================================================
   BUDDY-COMPACT-SENTINEL: Linux Kernel Buddy Allocator & Compaction Auditor   
===============================================================================
 Global Free RAM:   2.82 GiB  |  Contiguous 2MB+ Free:   2.37 GiB  |  PID: 11572
 Order 9 (2MB THP) Frag Index:  66.6% [MODERATE FRAGMENTATION]  |  THP: enabled=[always] defrag=[madvise]
 Compaction Stall Status: [NOMINAL: ZERO DIRECT COMPACTION STALLS DETECTED]
-------------------------------------------------------------------------------
 BUDDY ALLOCATOR 11-ORDER CONTIGUITY PYRAMID (Zone Normal / DMA32)
 ORDER | BLOCK SIZE | FREE BLOCKS | TOTAL MEMORY | CONTIGUOUS DISTRIBUTION
-------+------------+-------------+--------------+------------------------
     0 | 4 KiB (Base)|         47 |      188 KiB | ██                      
     1 | 8 KiB      |         10 |       80 KiB | █                       
     2 | 16 KiB     |        468 |      7.3 MiB | ████████████████████████
     3 | 32 KiB     |        387 |     12.1 MiB | ████████████████████    
     4 | 64 KiB     |        310 |     19.4 MiB | ████████████████        
     5 | 128 KiB    |        220 |     27.5 MiB | ███████████             
     6 | 256 KiB    |        146 |     36.5 MiB | ███████                 
     7 | 512 KiB    |         84 |     42.0 MiB | ████                    
     8 | 1024 KiB(1M)|         15 |     15.0 MiB | █                       
     9 | 2048 KiB(THP|         11 |     22.0 MiB | █                       
    10 | 4096 KiB(4M)|        262 |      1.0 GiB | █████████████           
-------------------------------------------------------------------------------
 DIRECT COMPACTION & THP LATENCY METRICS (Cumulative / Δ Delta)
 compact_stall:                  0 (+0)  | thp_fault_alloc:                 1155 (+0)
 compact_fail:                   0 (+0)  | thp_fault_fallback:              0 (+0)
 compact_success:                0 (+0)  | thp_collapse_alloc:              0 (+0)
 compact_daemon_wake:            0 (+0)  | allocstall_total:                0 (+0)
===============================================================================
 Telemetry: http://0.0.0.0:9115/metrics  | Refresh: 2.0s  | Press Ctrl+C to exit
```

---

## Prometheus Telemetry Endpoint

`buddy-compact-sentinel` embeds a zero-allocation HTTP server exporting Prometheus metrics on port `:9115`:

```prometheus
# HELP buddy_global_free_bytes Total free memory managed by the Linux Buddy Allocator in bytes
# TYPE buddy_global_free_bytes gauge
buddy_global_free_bytes 3025072128

# HELP buddy_global_hugepage_free_bytes Free memory available in contiguous 2MB+ blocks (Order 9 & 10)
# TYPE buddy_global_hugepage_free_bytes gauge
buddy_global_hugepage_free_bytes 2545942528

# HELP buddy_max_fragmentation_index_order9 Peak External Fragmentation Index for 2MB HugePages (0.0 to 1.0)
# TYPE buddy_max_fragmentation_index_order9 gauge
buddy_max_fragmentation_index_order9 0.6663

# HELP buddy_thp_exhausted_active 1 if Order 9 2MB HugePages are severely fragmented (>85% index)
# TYPE buddy_thp_exhausted_active gauge
buddy_thp_exhausted_active 0

# HELP buddy_compaction_stalling_active 1 if Direct Memory Compaction stalls have occurred
# TYPE buddy_compaction_stalling_active gauge
buddy_compaction_stalling_active 0

# HELP buddy_compact_stall_total Cumulative allocations that stalled in Direct Memory Compaction
# TYPE buddy_compact_stall_total counter
buddy_compact_stall_total 0

# HELP buddy_compact_fail_total Cumulative Direct Compaction attempts that failed to find contiguous space
# TYPE buddy_compact_fail_total counter
buddy_compact_fail_total 0

# HELP buddy_compact_success_total Cumulative Direct Compaction attempts that succeeded
# TYPE buddy_compact_success_total counter
buddy_compact_success_total 0

# HELP buddy_thp_fault_alloc_total Transparent HugePages successfully allocated on page fault
# TYPE buddy_thp_fault_alloc_total counter
buddy_thp_fault_alloc_total 1155

# HELP buddy_thp_fault_fallback_total Times THP allocation failed due to fragmentation and fell back to 4KB
# TYPE buddy_thp_fault_fallback_total counter
buddy_thp_fault_fallback_total 0

# HELP buddy_zone_free_blocks Free memory blocks per NUMA node, zone, and power-of-two page order (0..10)
# TYPE buddy_zone_free_blocks gauge
buddy_zone_free_blocks{node="0",zone="Normal",order="9"} 11
buddy_zone_free_blocks{node="0",zone="Normal",order="10"} 262
```

---

## Quickstart

### Build from Source
```bash
git clone https://github.com/patrick-isles/buddy-compact-sentinel.git
cd buddy-compact-sentinel
cargo build --release
```

### Run Interactively (TUI)
```bash
./target/release/buddy-compact-sentinel
```

### Run as Headless Telemetry Daemon
```bash
./target/release/buddy-compact-sentinel --daemon --port 9115 &
curl http://127.0.0.1:9115/metrics
```

### CLI Options
```
buddy-compact-sentinel [OPTIONS]

OPTIONS:
    --port <PORT>        Prometheus HTTP exporter port (default: 9115)
    --interval <SECS>    Terminal refresh interval in seconds (default: 2.0)
    --daemon             Run in headless daemon mode (no terminal TUI)
    -h, --help           Show this help message
```

---

## Production Deployment (systemd)

Install the hardened systemd unit file:

```bash
sudo cp target/release/buddy-compact-sentinel /usr/local/bin/
sudo cp systemd/buddy-compact.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now buddy-compact.service
```

Verify service telemetry:
```bash
systemctl status buddy-compact.service
curl -s http://127.0.0.1:9115/metrics | grep buddy_max_fragmentation_index
```

---

## Author

**Patrick Isles**  
*Systems & Low-Latency Infrastructure Engineer*  
Portland, Oregon | [patrick.isles@pcc.edu](mailto:patrick.isles@pcc.edu)  
GitHub: [@patrick-isles](https://github.com/patrick-isles)
