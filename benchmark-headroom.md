# Server-side throughput headroom (2026-08-26)

Question this answers: where is the wall - in the engine or in the
network?

## Hardware and method

- Beelink mini-PC: Intel i7, 32 GB RAM, 1 TB NVMe SSD (Crucial P3 Plus),
  Ubuntu 24.04. Clarus + clbridge in Docker (Portainer stack, images
  `clarus:local` / `clbridge:local`).
- Loopback (`127.0.0.1:8024`) - no NIC in the path.
- Corpus: two studies, 68,421,394 B (MR, 340 slices) + 77,262,352 B
  (CT, 368 slices) = 145,683,746 B total.
- warm: page cache (studies ingested minutes before the run).
- cold: `echo 3 > /proc/sys/vm/drop_caches` before each GET - real NVMe
  reads, not a synthetic sequential scan.
- `curl -s -o /dev/null -w` with `time_total` / `size_download` /
  `speed_download`; 3 sequential passes per study, plus parallel runs.

## Results

| Condition | Single stream | Two streams (aggregate) |
|---|---|---|
| Warm (page cache) | 0.067-0.093 s per study, 1.03-1.11 GB/s steady | wall 0.208 s, 668 MiB/s |
| Cold (SSD, drop_caches) | 0.180-0.228 s per study, 344-428 MB/s | wall 0.253 s, 549 MiB/s |

- Hottest single GET: 77.3 MB in 0.058 s = 1.33 GB/s.
- Storage consistency: `du -sb` of the data volume = 145,683,746 B =
  the exact sum of the two studies' instance bytes (no index files
  inside the data volume).

## Field cross-check (the honest part)

A dual C-MOVE end to end to a Weasis viewer on the same LAN moved the
same 145.76 MB in ~16 s wall = 9.1 MB/s aggregate. Both NICs are
Realtek GbE chips but both links had negotiated 100 Mbit/s
(`enp171s0 speed=100`; Windows side `100000000`) - cable/switch suspect,
under investigation. The server-side request times in that run were
stretched by the 100 Mbit/s backpressure: the same study leaves loopback
in 0.058 s. The server was waiting for the network, not the other way
around.

## Caveats, stated flatly

- Warm numbers are page-cache serving - typical for studies ingested in
  the last days; older studies run at the cold rate.
- Cold numbers are NVMe small-file reads (708 files, ~200 KB each)
  through the full serve path - per-file open/parse dominates, so the
  cold floor is IOPS-bound, not media-bound; a different NVMe would not
  move it much, page cache does.
- Both are server-side loopback measurements - a real NIC caps lower
  (see the cross-check above).
- A GbE re-run after the cable/switch fix is planned; numbers will be
  appended here.

## Trademarks

DICOM® is the registered trademark of the National Electrical Manufacturers
Association (NEMA) for its standards publications relating to digital
communications of medical information. DICOMweb™ is a trademark of NEMA.
Clarus is not affiliated with or endorsed by NEMA.
