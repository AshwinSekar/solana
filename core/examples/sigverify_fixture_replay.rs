//! Replay a previously generated slot-based synthetic sigverify fixture through the
//! real sigverifier packet channel.

#![allow(clippy::arithmetic_side_effects)]

extern crate clap4 as clap;

#[path = "support/sigverify_fixture_common.rs"]
mod sigverify_fixture_common;

use {
    agave_bls_sigverify::bls_sigverifier::PerSlotTiming,
    clap::{Parser, ValueEnum},
    rand::{Rng, SeedableRng, rngs::StdRng},
    sigverify_fixture_common::{
        ExampleContext, StoredPacket, StoredPacketKind, StoredWorkload, fixture_max_slot,
        init_example_context,
    },
    solana_perf::packet::{Packet, PacketBatch, RecycledPacketBatch},
    std::{
        fs::File,
        io::BufReader,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
};

const SEND_BLOCK_WARN_US: u64 = 100;

#[derive(Debug, Clone, Copy, ValueEnum, serde::Serialize)]
pub enum ReplayArrivalPattern {
    /// Use arrival_us from the fixture file as-is.
    Stored,

    /// Reassign votes and certs uniformly across each slot window.
    Uniform,

    /// Reassign votes uniformly, but place certs into random bursts.
    CertBursts,
}

#[derive(Parser)]
pub struct ReplayConfig {
    #[arg(long, help = "Emit CSV output instead of human-readable output")]
    pub csv: bool,

    #[arg(
        long = "input",
        help = "Path to a previously generated workload fixture"
    )]
    pub input: String,

    #[arg(
        long = "arrival-pattern",
        value_enum,
        default_value = "cert-bursts",
        help = "Replay arrival layout: stored, uniform, or cert-bursts"
    )]
    pub arrival_pattern: ReplayArrivalPattern,

    #[arg(
        long = "arrival-seed",
        default_value_t = 0,
        help = "Seed for replay-only arrival reshuffling; 0 derives it from workload.seed"
    )]
    pub arrival_seed: u64,

    #[arg(
        long = "cert-bursts-per-slot",
        default_value_t = 20,
        help = "Number of cert arrival bursts generated per slot for cert-bursts layout"
    )]
    pub cert_bursts_per_slot: usize,

    #[arg(
        long = "cert-burst-jitter-us",
        default_value_t = 500,
        help = "Maximum +/- jitter around each cert burst center in microseconds"
    )]
    pub cert_burst_jitter_us: u64,

    #[arg(
        long = "batch-window-us",
        default_value_t = 1600,
        help = "Time window in microseconds used to collect arrived packets into one PacketBatch"
    )]
    pub batch_window_us: u64,

    #[arg(
        long = "max-packets-per-batch",
        default_value_t = 1024,
        help = "Maximum number of packets emitted in one synthetic PacketBatch"
    )]
    pub max_packets_per_batch: usize,

    #[arg(
        long = "num-threads",
        help = "Thread count for the verifier thread pool"
    )]
    pub num_threads: usize,

    #[arg(
        long = "debug-batches",
        default_value_t = 0,
        help = "Print the first N timed batches before and during replay"
    )]
    pub debug_batches: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct OutputRow {
    pub seed: u64,
    pub arrival_pattern: ReplayArrivalPattern,
    pub batch_window_us: u64,
    pub max_packets_per_batch: usize,
    pub emitted_batches: usize,
    pub avg_packets_per_batch: f64,
    pub num_slots: usize,
    pub votes_per_slot: usize,
    pub certs_per_slot: usize,
    pub base_slot: u64,
    pub slot_window_us: u64,
    pub cert_signers: usize,
    pub cert_ratio: f64,
    pub vote_ratio: f64,
    pub num_threads: usize,
    pub num_validators: usize,
    pub total_packets: usize,
    pub vote_packets: usize,
    pub cert_packets: usize,

    pub sigverify_total_us: u64,
    pub sigverify_avg_us_per_slot: f64,
    pub sigverify_max_us_per_slot: u64,
    pub sigverify_max_slot: u64,
    pub sigverify_threads_needed_avg: f64,
    pub sigverify_threads_needed_max: f64,

    pub elapsed_us: u64,
    pub per_packet_us: u64,
}

pub struct TimedBatch {
    pub send_at_us: u64,
    pub batch: PacketBatch,
}

pub struct TimedBatchDebug {
    pub index: usize,
    pub send_at_us: u64,
    pub packet_count: usize,
    pub vote_count: usize,
    pub cert_count: usize,
    pub first_arrival_us: u64,
    pub last_arrival_us: u64,
}

fn validate_replay_config(config: &ReplayConfig) -> Result<(), String> {
    if config.batch_window_us == 0 {
        return Err("batch_window_us must be > 0".to_string());
    }

    if config.max_packets_per_batch == 0 {
        return Err("max_packets_per_batch must be > 0".to_string());
    }

    if config.num_threads == 0 {
        return Err("num_threads must be > 0".to_string());
    }

    if matches!(config.arrival_pattern, ReplayArrivalPattern::CertBursts)
        && config.cert_bursts_per_slot == 0
    {
        return Err("cert_bursts_per_slot must be > 0 for cert-bursts".to_string());
    }

    Ok(())
}

fn replay_arrival_seed(workload: &StoredWorkload, config: &ReplayConfig) -> u64 {
    if config.arrival_seed == 0 {
        workload.seed ^ 0xCE17_BAAD_D157_1B11
    } else {
        config.arrival_seed
    }
}

fn slot_rng_seed(seed: u64, slot_index: usize) -> u64 {
    seed ^ (slot_index as u64).wrapping_mul(0xA076_1D64_78BD_642F)
}

fn uniform_arrival_us(rng: &mut StdRng, slot_start_us: u64, slot_window_us: u64) -> u64 {
    slot_start_us + rng.random_range(0..slot_window_us)
}

fn burst_arrival_us(
    rng: &mut StdRng,
    slot_start_us: u64,
    slot_window_us: u64,
    burst_centers: &[u64],
    burst_jitter_us: u64,
) -> u64 {
    let center = burst_centers[rng.random_range(0..burst_centers.len())];

    let jitter_span = burst_jitter_us.saturating_mul(2).saturating_add(1);
    let jitter = rng.random_range(0..jitter_span) as i64 - burst_jitter_us as i64;

    let arrival_offset =
        (center as i64 + jitter).clamp(0, slot_window_us.saturating_sub(1) as i64) as u64;

    slot_start_us + arrival_offset
}

fn reshuffle_workload_for_replay(
    workload: &StoredWorkload,
    config: &ReplayConfig,
) -> StoredWorkload {
    if matches!(config.arrival_pattern, ReplayArrivalPattern::Stored) {
        return workload.clone();
    }

    let seed = replay_arrival_seed(workload, config);
    let mut packets = workload.packets.clone();

    let mut slot_rngs: Vec<StdRng> = (0..workload.num_slots)
        .map(|slot_index| StdRng::seed_from_u64(slot_rng_seed(seed, slot_index)))
        .collect();

    let cert_burst_centers_by_slot: Vec<Vec<u64>> = (0..workload.num_slots)
        .map(|slot_index| {
            let mut rng = StdRng::seed_from_u64(slot_rng_seed(seed ^ 0xC347_B015, slot_index));

            (0..config.cert_bursts_per_slot)
                .map(|_| rng.random_range(0..workload.slot_window_us))
                .collect()
        })
        .collect();

    for packet in packets.iter_mut() {
        let slot_index = packet.slot_index;
        let slot_start_us = slot_index as u64 * workload.slot_window_us;
        let rng = &mut slot_rngs[slot_index];

        packet.arrival_us = match config.arrival_pattern {
            ReplayArrivalPattern::Stored => packet.arrival_us,
            ReplayArrivalPattern::Uniform => {
                uniform_arrival_us(rng, slot_start_us, workload.slot_window_us)
            }
            ReplayArrivalPattern::CertBursts => match packet.kind {
                StoredPacketKind::Vote => {
                    uniform_arrival_us(rng, slot_start_us, workload.slot_window_us)
                }
                StoredPacketKind::Cert => burst_arrival_us(
                    rng,
                    slot_start_us,
                    workload.slot_window_us,
                    &cert_burst_centers_by_slot[slot_index],
                    config.cert_burst_jitter_us,
                ),
            },
        };
    }

    packets.sort_by_key(|packet| packet.arrival_us);

    StoredWorkload {
        seed: workload.seed,
        num_slots: workload.num_slots,
        votes_per_slot: workload.votes_per_slot,
        certs_per_slot: workload.certs_per_slot,
        base_slot: workload.base_slot,
        slot_window_us: workload.slot_window_us,
        cert_signers: workload.cert_signers,
        num_validators: workload.num_validators,
        total_packets: workload.total_packets,
        vote_packets: workload.vote_packets,
        cert_packets: workload.cert_packets,
        packets,
    }
}

fn stored_packet_to_packet(stored: StoredPacket) -> Packet {
    let mut packet = Packet::default();
    let data_len = stored.message_bytes.len();

    packet.buffer_mut()[..data_len].copy_from_slice(&stored.message_bytes);
    packet.meta_mut().size = data_len;
    packet.meta_mut().set_remote_pubkey(stored.remote_pubkey);

    packet
}

fn make_timed_batches(
    workload: &StoredWorkload,
    batch_window_us: u64,
    max_packets_per_batch: usize,
) -> Vec<TimedBatch> {
    assert!(batch_window_us > 0);
    assert!(max_packets_per_batch > 0);

    if workload.packets.is_empty() {
        return Vec::new();
    }

    let mut timed_batches = Vec::new();
    let mut packet_index = 0;

    while packet_index < workload.packets.len() {
        let first_packet_arrival_us = workload.packets[packet_index].arrival_us;
        let window_start_us = first_packet_arrival_us - (first_packet_arrival_us % batch_window_us);
        let window_end_us = window_start_us.saturating_add(batch_window_us);

        while packet_index < workload.packets.len()
            && workload.packets[packet_index].arrival_us < window_end_us
        {
            let mut packets = Vec::with_capacity(max_packets_per_batch);

            while packet_index < workload.packets.len()
                && workload.packets[packet_index].arrival_us < window_end_us
                && packets.len() < max_packets_per_batch
            {
                packets.push(stored_packet_to_packet(
                    workload.packets[packet_index].clone(),
                ));
                packet_index += 1;
            }

            if !packets.is_empty() {
                timed_batches.push(TimedBatch {
                    send_at_us: window_end_us,
                    batch: RecycledPacketBatch::new(packets).into(),
                });
            }
        }
    }

    timed_batches
}

fn debug_timed_batches(
    workload: &StoredWorkload,
    batch_window_us: u64,
    max_packets_per_batch: usize,
    max_batches: usize,
) -> Vec<TimedBatchDebug> {
    if max_batches == 0 || workload.packets.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut packet_index = 0;
    let mut batch_index = 0;

    while packet_index < workload.packets.len() && result.len() < max_batches {
        let first_packet_arrival_us = workload.packets[packet_index].arrival_us;
        let window_start_us = first_packet_arrival_us - (first_packet_arrival_us % batch_window_us);
        let window_end_us = window_start_us.saturating_add(batch_window_us);

        while packet_index < workload.packets.len()
            && workload.packets[packet_index].arrival_us < window_end_us
            && result.len() < max_batches
        {
            let first_arrival_us = workload.packets[packet_index].arrival_us;
            let mut last_arrival_us = first_arrival_us;
            let mut packet_count = 0usize;
            let mut vote_count = 0usize;
            let mut cert_count = 0usize;

            while packet_index < workload.packets.len()
                && workload.packets[packet_index].arrival_us < window_end_us
                && packet_count < max_packets_per_batch
            {
                match workload.packets[packet_index].kind {
                    StoredPacketKind::Vote => vote_count += 1,
                    StoredPacketKind::Cert => cert_count += 1,
                }

                packet_count += 1;
                last_arrival_us = workload.packets[packet_index].arrival_us;
                packet_index += 1;
            }

            if packet_count > 0 {
                result.push(TimedBatchDebug {
                    index: batch_index,
                    send_at_us: window_end_us,
                    packet_count,
                    vote_count,
                    cert_count,
                    first_arrival_us,
                    last_arrival_us,
                });

                batch_index += 1;
            }
        }
    }

    result
}

fn load_workload_from_file<P: AsRef<Path>>(
    path: P,
) -> Result<StoredWorkload, Box<dyn std::error::Error>> {
    let reader = BufReader::new(File::open(path)?);
    let workload = bincode::deserialize_from(reader)?;
    Ok(workload)
}

fn print_results(row: &OutputRow, csv_output: bool) {
    if csv_output {
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .from_writer(std::io::stdout());

        writer.serialize(row).expect("failed to serialize CSV row");
        writer.flush().expect("failed to flush CSV writer");
    } else {
        println!("{row:#?}");
    }
}

fn wait_until(start: Instant, send_at_us: u64) {
    let target = start + Duration::from_micros(send_at_us);

    loop {
        let now = Instant::now();

        if now >= target {
            return;
        }

        let remaining = target.saturating_duration_since(now);

        if remaining > Duration::from_micros(100) {
            thread::sleep(remaining / 2);
        } else {
            std::hint::spin_loop();
        }
    }
}

fn elapsed_us_since(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

fn main() {
    let config = ReplayConfig::parse();

    validate_replay_config(&config).unwrap_or_else(|err| {
        eprintln!("error: {err}");
        std::process::exit(1);
    });

    let workload = load_workload_from_file(&config.input).unwrap_or_else(|err| {
        eprintln!("failed to load workload: {err}");
        std::process::exit(1);
    });

    let workload = reshuffle_workload_for_replay(&workload, &config);
    let timed_batches = make_timed_batches(
        &workload,
        config.batch_window_us,
        config.max_packets_per_batch,
    );

    let emitted_batches = timed_batches.len();
    let avg_packets_per_batch = if emitted_batches == 0 {
        0.0
    } else {
        workload.total_packets as f64 / emitted_batches as f64
    };

    let scheduled_end_us = timed_batches
        .last()
        .map(|timed_batch| timed_batch.send_at_us)
        .unwrap_or_default();

    if config.debug_batches > 0 {
        eprintln!(
            "debug: first {} timed batches after replay reshuffle:",
            config.debug_batches,
        );

        for batch in debug_timed_batches(
            &workload,
            config.batch_window_us,
            config.max_packets_per_batch,
            config.debug_batches,
        ) {
            eprintln!(
                "debug batch #{:04}: send_at_us={}, packets={}, votes={}, certs={}, \
                 first_arrival_us={}, last_arrival_us={}",
                batch.index,
                batch.send_at_us,
                batch.packet_count,
                batch.vote_count,
                batch.cert_count,
                batch.first_arrival_us,
                batch.last_arrival_us,
            );
        }
    }

    eprintln!(
        "Prepare phase is over; Start paced replay (arrival_pattern={:?}, emitted_batches={}, \
         avg_packets_per_batch={:.2}, scheduled_end_us={})",
        config.arrival_pattern, emitted_batches, avg_packets_per_batch, scheduled_end_us,
    );

    let max_slot = fixture_max_slot(workload.base_slot, workload.num_slots);
    let ctx = init_example_context(
        config.num_threads,
        workload.num_validators,
        workload.seed,
        workload.base_slot,
        max_slot,
    );

    let ExampleContext {
        verifier,
        packet_sender,
        validator_keypairs: _validator_keypairs,
        validator_ranks: _validator_ranks,
        _repair_receiver,
        _reward_receiver,
        _pool_receiver,
        _metrics_receiver,
    } = ctx;

    let exit = Arc::new(AtomicBool::new(false));
    let verifier_exit = Arc::clone(&exit);

    let timing = PerSlotTiming::new(workload.base_slot, workload.num_slots);

    let verifier_thread = thread::Builder::new()
        .name("sigverify-fixture-replay".to_string())
        .spawn(move || {
            let mut timing = timing;
            verifier.run_with_per_slot_timing(verifier_exit, &mut timing);
            timing
        })
        .expect("failed to spawn verifier thread");

    let replay_start = Instant::now();

    let mut max_schedule_lag_us = 0u64;
    let mut max_send_block_us = 0u64;
    let mut total_send_block_us = 0u64;
    let mut blocked_sends_over_100us = 0u64;
    let mut actual_send_end_us = 0u64;

    for (index, timed_batch) in timed_batches.into_iter().enumerate() {
        wait_until(replay_start, timed_batch.send_at_us);

        let before_send_us = elapsed_us_since(replay_start);
        let schedule_lag_us = before_send_us.saturating_sub(timed_batch.send_at_us);

        let send_start = Instant::now();
        packet_sender
            .send(timed_batch.batch)
            .expect("packet receiver disconnected");
        let send_block_us = send_start.elapsed().as_micros() as u64;

        actual_send_end_us = elapsed_us_since(replay_start);
        max_schedule_lag_us = max_schedule_lag_us.max(schedule_lag_us);
        max_send_block_us = max_send_block_us.max(send_block_us);
        total_send_block_us = total_send_block_us.saturating_add(send_block_us);

        if send_block_us > SEND_BLOCK_WARN_US {
            blocked_sends_over_100us = blocked_sends_over_100us.saturating_add(1);
        }

        if index < config.debug_batches {
            eprintln!(
                "debug send #{:04}: scheduled_send_at_us={}, before_send_us={}, \
                 actual_send_end_us={}, schedule_lag_us={}, send_block_us={}",
                index,
                timed_batch.send_at_us,
                before_send_us,
                actual_send_end_us,
                schedule_lag_us,
                send_block_us,
            );
        }
    }

    while packet_sender.len() > 0 {
        thread::sleep(Duration::from_millis(1));
    }

    exit.store(true, Ordering::Relaxed);
    drop(packet_sender);

    let timing = verifier_thread
        .join()
        .expect("verifier thread panicked during replay");

    let timing_summary = timing.summary();

    let elapsed_us = elapsed_us_since(replay_start);

    eprintln!(
        "send schedule: scheduled_end_us={}, actual_send_end_us={}, send_lag_us={}, \
         max_schedule_lag_us={}, max_send_block_us={}, total_send_block_us={}, \
         blocked_sends_over_100us={}",
        scheduled_end_us,
        actual_send_end_us,
        actual_send_end_us.saturating_sub(scheduled_end_us),
        max_schedule_lag_us,
        max_send_block_us,
        total_send_block_us,
        blocked_sends_over_100us,
    );

    let cert_ratio = if workload.total_packets == 0 {
        0.0
    } else {
        workload.cert_packets as f64 / workload.total_packets as f64
    };

    let vote_ratio = if workload.total_packets == 0 {
        0.0
    } else {
        workload.vote_packets as f64 / workload.total_packets as f64
    };

    let per_packet_us = if workload.total_packets == 0 {
        0
    } else {
        elapsed_us / workload.total_packets as u64
    };

    let sigverify_threads_needed_avg = if workload.slot_window_us == 0 {
        0.0
    } else {
        timing_summary.avg_us_per_slot / workload.slot_window_us as f64
    };

    let sigverify_threads_needed_max = if workload.slot_window_us == 0 {
        0.0
    } else {
        timing_summary.max_us_per_slot as f64 / workload.slot_window_us as f64
    };

    let row = OutputRow {
        seed: workload.seed,
        arrival_pattern: config.arrival_pattern,
        batch_window_us: config.batch_window_us,
        max_packets_per_batch: config.max_packets_per_batch,
        emitted_batches,
        avg_packets_per_batch,
        num_slots: workload.num_slots,
        votes_per_slot: workload.votes_per_slot,
        certs_per_slot: workload.certs_per_slot,
        base_slot: workload.base_slot,
        slot_window_us: workload.slot_window_us,
        cert_signers: workload.cert_signers,
        cert_ratio,
        vote_ratio,
        num_threads: config.num_threads,
        num_validators: workload.num_validators,
        total_packets: workload.total_packets,
        vote_packets: workload.vote_packets,
        cert_packets: workload.cert_packets,

        sigverify_total_us: timing_summary.total_us,
        sigverify_avg_us_per_slot: timing_summary.avg_us_per_slot,
        sigverify_max_us_per_slot: timing_summary.max_us_per_slot,
        sigverify_max_slot: timing_summary.max_slot,
        sigverify_threads_needed_avg,
        sigverify_threads_needed_max,

        elapsed_us,
        per_packet_us,
    };

    print_results(&row, config.csv);
}
