use {
    clap::{crate_description, crate_name, value_t, value_t_or_exit, App, Arg},
    itertools::Itertools,
    log::{info, trace},
    rand::{
        distributions::{Distribution, WeightedIndex},
        Rng,
    },
    rayon::{prelude::*, ThreadPool, ThreadPoolBuilder},
    solana_client::{
        rpc_client::RpcClient,
        rpc_response::{RpcVoteAccountInfo, RpcVoteAccountStatus},
    },
    solana_gossip::{cluster_info::compute_retransmit_peers, weighted_shuffle::WeightedShuffle},
    solana_logger,
    solana_sdk::pubkey::Pubkey,
    std::{
        cmp::Reverse,
        collections::{HashMap, HashSet, VecDeque},
        iter::{repeat, repeat_with},
        str::FromStr,
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

const API_MAINNET_BETA: &str = "https://api.mainnet-beta.solana.com";

#[derive(Copy, Clone, Debug)]
struct Config {
    num_reps: usize,
    node_index: usize,
    fanout: usize,
    batch_size: usize,
    min_num_shards: usize,
    node_fault_rate: f64,
    edge_fault_rate: f64,
    disable_parent_link: bool,
    disable_first_node_link: bool,
    stake_threshold: f64,
    num_threads: usize,
}

// Obtains nodes stakes distribution from mainnet cluster.
fn get_staked_nodes(rpc_client: &RpcClient) -> Vec<(/*node:*/ Pubkey, /*stake:*/ u64)> {
    let vote_accounts: RpcVoteAccountStatus = rpc_client.get_vote_accounts().unwrap();
    let sum_activated_stake = |vote_accounts: &[RpcVoteAccountInfo]| {
        vote_accounts
            .iter()
            .filter(|info| info.epoch_vote_account)
            .map(|info| info.activated_stake)
            .sum()
    };
    let current_stake: u64 = sum_activated_stake(&vote_accounts.current);
    let delinquent_stake: u64 = sum_activated_stake(&vote_accounts.delinquent);
    info!("current    stake: {:19}", current_stake);
    info!("delinquent stake: {:19}", delinquent_stake);
    info!(
        "delinquent: {:.2}%",
        delinquent_stake as f64 / ((current_stake + delinquent_stake) as f64) * 100.0
    );
    let mut stakes: Vec<(Pubkey, /*stake:*/ u64)> = vote_accounts
        .current
        .iter()
        .chain(&vote_accounts.delinquent)
        .filter(|info| info.epoch_vote_account)
        .into_grouping_map_by(|info| Pubkey::from_str(&info.node_pubkey).unwrap())
        .aggregate(|stake, _node_pubkey, vote_account_info| {
            Some(stake.unwrap_or_default() + vote_account_info.activated_stake)
        })
        .into_iter()
        .filter(|(_, stake)| *stake != 0)
        .collect();
    stakes.sort_unstable_by_key(|(pubkey, stake)| (Reverse(*stake), *pubkey));
    info!("number of staked nodes: {}", stakes.len());
    {
        let (node, stake) = stakes.first().unwrap();
        trace!("node: {}, stake: {}", node, stake);
        let (node, stake) = stakes.last().unwrap();
        trace!("node: {}, stake: {}", node, stake);
    }
    stakes
}

// Returns a sample of nodes which will be marked as dead and together will
// have ~node_fault_rate of total stake.
// Excludes node_index from set of dead nodes.
fn sample_dead_nodes<R: Rng>(rng: &mut R, config: &Config, stakes: &[u64]) -> HashSet<usize> {
    assert!(config.stake_threshold + config.node_fault_rate <= 1.0);
    // TODO this is done after removing leader node!
    let num_nodes = stakes.len();
    let cluster_stake = stakes.iter().sum::<u64>();
    let mut faulty_stake = cluster_stake as f64 * config.node_fault_rate;
    let mut out = HashSet::new();
    while faulty_stake > 0.0 {
        let candidates: Vec<_> = stakes
            .iter()
            .enumerate()
            .filter(|(i, stake)| {
                !out.contains(i) && **stake <= faulty_stake as u64 && *i != config.node_index
            })
            .collect();
        if candidates.len() == 0 {
            break;
        }
        let (index, stake) = candidates[rng.gen_range(0usize, candidates.len())];
        out.insert(index);
        faulty_stake -= *stake as f64;
    }
    trace!(
        "number of dead nodes: {}/{} faulty stake {} % vs requested {} %",
        out.len(),
        num_nodes,
        100 as f64 * ((cluster_stake as f64 * config.node_fault_rate - faulty_stake) as f64)
            / (cluster_stake as f64),
        100 as f64 * config.node_fault_rate
    );
    out
}

// Returns a two dimensional vector where out[i][j] is the probability that the
// cluster (as defined by config.stake_threshold) receives j shreds from a
// batch if our node receives i shreds from the batch.
fn run_turbine(
    config: &Config,
    stakes: &Vec<u64>,
    thread_pool: &ThreadPool,
) -> (
    Vec<f64>, // Probability that our nodes receives k shreds.
    Vec<Vec<f64>>,
) {
    let cluster_stake = stakes.iter().sum::<u64>() as f64;
    let size = config.batch_size + 1;
    // Number of times our node received k shreds from the batch.
    let counts: Vec<_> = repeat_with(|| AtomicUsize::default()).take(size).collect();
    // Amount of stake that was able to recover the block
    let recovered_stake: Vec<_> = repeat_with(|| AtomicU64::default())
        .take(config.num_reps)
        .collect();
    let recovered_count: Vec<_> = repeat_with(|| AtomicUsize::default())
        .take(config.num_reps)
        .collect();
    // arr[i][j] number of times config.stake_threshold of the cluster received
    // the at least j shreds from the batch when our node received i shreds
    // from the batch.
    let arr: Vec<Vec<_>> =
        repeat_with(|| repeat_with(|| AtomicUsize::default()).take(size).collect())
            .take(size)
            .collect();
    thread_pool.install(|| {
        (0..config.num_reps)
            .into_par_iter()
            .for_each_init(rand::thread_rng, |rng, trial| {
                // nodes_stake[k] is the total amount of stake that received at least
                // k shreds from the batch.
                // nodes_count[k] is the # of nodes that received at least k shreds from the batch
                let (num_shreds, nodes_stake, nodes_count) =
                    run_batch(rng, config.clone(), stakes.clone());
                counts[num_shreds].fetch_add(1, Ordering::Relaxed);
                recovered_stake[trial].fetch_add(nodes_stake[32], Ordering::Relaxed);
                recovered_count[trial].fetch_add(nodes_count[32], Ordering::Relaxed);
                for i in 0..size {
                    if nodes_stake[i] as f64 / cluster_stake > config.stake_threshold {
                        arr[num_shreds][i].fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
    });
    let mut counts: Vec<_> = counts.into_iter().map(AtomicUsize::into_inner).collect();
    let mut arr: Vec<Vec<_>> = arr
        .into_iter()
        .map(|row| row.into_iter().map(AtomicUsize::into_inner).collect())
        .collect();
    for i in (0..config.batch_size).rev() {
        counts[i] += counts[i + 1];
        for j in 0..size {
            arr[i][j] += arr[i + 1][j];
        }
    }
    let mut rows = vec![0f64; size];
    for i in 0..size {
        rows[i] += counts[i] as f64 / config.num_reps as f64;
    }
    let mut out = vec![vec![0f64; size]; size];
    for i in 0..size {
        for j in 0..size {
            if counts[i] != 0 {
                out[i][j] = arr[i][j] as f64 / counts[i] as f64;
            }
        }
    }
    let mut recovered: Vec<_> = recovered_stake
        .into_iter()
        .map(AtomicU64::into_inner)
        .collect();
    let mut recovered_count: Vec<_> = recovered_count
        .into_iter()
        .map(AtomicUsize::into_inner)
        .collect();
    recovered.sort();
    recovered_count.sort();
    println!(
        "Median recovery stake % {} node count % {}",
        100f64 * recovered[config.num_reps / 2] as f64 / cluster_stake,
        100f64 * recovered_count[config.num_reps / 2] as f64 / stakes.len() as f64
    );
    (rows, out)
}

// Simulates a single erasure batch. Returns:
// 1. number of shreds received at our node.
// 2. For each k in [0, batch_size], total amount of stake that received
//    at least k shreds from the batch.
fn run_batch<R: Rng>(
    rng: &mut R,
    mut config: Config,
    mut stakes: Vec<u64>,
) -> (
    usize,               // Number of shreds received at our node.
    Vec</*stake:*/ u64>, // Total stake that received at least k shreds.
    Vec<usize>,          // Total # of nodes that received at least k shreds.
) {
    // Randomly select a node as the leader.
    let leader_node_index = {
        let weighted_index = WeightedIndex::new(&stakes).unwrap();
        repeat_with(|| weighted_index.sample(rng))
            .skip_while(|&index| index == config.node_index)
            .next()
            .unwrap()
    };
    // Remove the leader from the retransmit nodes.
    stakes.remove(leader_node_index);
    if config.node_index > leader_node_index {
        config.node_index -= 1;
    };
    // Randomly mark node_fault_rate of stake as dead.
    // Same set of nodes are dead for the entire batch.
    let dead_nodes = sample_dead_nodes(rng, &config, &stakes);
    let weighted_shuffle = WeightedShuffle::new(&stakes).unwrap();
    // Number of shreds reached at each node.
    let mut shred_count = vec![0usize; stakes.len()];
    for _ in 0..config.batch_size {
        let nodes = run_shred(rng, &config, &dead_nodes, weighted_shuffle.clone());
        for node in nodes {
            assert!(!dead_nodes.contains(&node));
            shred_count[node] += 1;
        }
    }
    // TODO should also include leader nodes' stake.
    let mut out = vec![0u64; config.batch_size + 1];
    let mut count = vec![0usize; config.batch_size + 1];
    for (index, shreds) in shred_count.iter().enumerate() {
        out[*shreds] += stakes[index];
        count[*shreds] += 1;
    }
    for i in (0..config.batch_size).rev() {
        out[i] += out[i + 1];
        count[i] += count[i + 1];
    }
    (shred_count[config.node_index], out, count)
}

// Simulates a single shred broadcast.
// Returns node indices which received the shred.
fn run_shred<R: Rng>(
    rng: &mut R,
    config: &Config,
    dead_nodes: &HashSet<usize>,
    weighted_shuffle: WeightedShuffle<u64>,
) -> HashSet<usize> {
    let nodes: Vec<_> = weighted_shuffle.shuffle(rng).collect();
    // Reverse lookup index.
    let index: HashMap<_, _> = nodes.iter().enumerate().map(|(i, j)| (*j, i)).collect();
    // The very first node of the broadcast tree. It should always retransmit
    // to both its children and neighbors, otherwise some nodes will never
    // receive the shreds.
    let root = nodes.first().copied().unwrap();
    assert_eq!(index[&root], 0);
    if dead_nodes.contains(&root) {
        return HashSet::default();
    }
    let mut out = HashSet::new();
    let mut queue = VecDeque::<usize>::new();
    out.insert(root);
    queue.push_back(root);
    while !queue.is_empty() {
        let node = queue.pop_front().unwrap();
        let node_index = index[&node];
        assert_eq!(nodes[node_index], node);
        assert!(!dead_nodes.contains(&node));
        let (neighbors, children) = compute_retransmit_peers(config.fanout, node_index, &nodes);
        let anchor = node_index % config.fanout == 0;
        assert!(neighbors.contains(&node));
        assert!(!children.contains(&node));
        assert!(!children.contains(&root));
        assert_eq!(anchor, neighbors[0] == node);
        if node == root || (anchor && !config.disable_first_node_link) {
            for peer in neighbors {
                if !dead_nodes.contains(&peer)
                    && !rng.gen_bool(config.edge_fault_rate)
                    && out.insert(peer)
                {
                    queue.push_back(peer)
                }
            }
        }
        if node == root || !config.disable_parent_link {
            for peer in children {
                if !dead_nodes.contains(&peer)
                    && !rng.gen_bool(config.edge_fault_rate)
                    && out.insert(peer)
                {
                    queue.push_back(peer)
                }
            }
        }
    }
    out
}

fn main() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "INFO");
    }
    solana_logger::setup();

    let matches = App::new(crate_name!())
        .about(crate_description!())
        .arg(
            Arg::with_name("num_reps")
                .long("num-reps")
                .takes_value(true)
                .default_value("1000")
                .help("number of times the simulation is repeated"),
        )
        .arg(
            Arg::with_name("node_index")
                .long("node-index")
                .takes_value(true)
                .required(true)
                .help(
                    "index of node in the cluster. \
                       0 is highest staked node, 1 is 2nd highest by stake, \
                       and so on ...",
                ),
        )
        .arg(
            Arg::with_name("fanout")
                .long("fanout")
                .takes_value(true)
                .default_value("200")
                .help("turbine data plan fanout size"),
        )
        .arg(
            Arg::with_name("batch_size")
                .long("batch-size")
                .takes_value(true)
                .default_value("64")
                .help("erasure batch size"),
        )
        .arg(
            Arg::with_name("min_num_shards")
                .long("min-num-shards")
                .takes_value(true)
                .help(
                    "minimum number of shards in an erasure batch required \
                       to recover the entire batch. This only impacts veiw \
                       of results printed at the end. It does not change the \
                       results.",
                ),
        )
        .arg(
            Arg::with_name("node_fault_rate")
                .long("node-fault-rate")
                .takes_value(true)
                .default_value("0.33")
                .help("fraction of stake which is dead."),
        )
        .arg(
            Arg::with_name("edge_fault_rate")
                .long("edge-fault-rate")
                .takes_value(true)
                .default_value("0.0")
                .help(
                    "the probability that the link between two nodes drops \
                       the packet/shred even though both ends are up.",
                ),
        )
        .arg(
            Arg::with_name("disable_parent_link")
                .long("disable-parent-link")
                .takes_value(false)
                .help(
                    "disable propagation from parent nodes to children; \
                       except for the very first node in the tree",
                ),
        )
        .arg(
            Arg::with_name("disable_first_node_link")
                .long("disable-first-node-link")
                .takes_value(false)
                .help(
                    "disable propagation from first node of each \
                       neighborhoods to other nodes in that neighborhood; \
                       except for the very frist node in the tree",
                ),
        )
        .arg(
            Arg::with_name("stake_threshold")
                .long("stake-threshold")
                .takes_value(true)
                .default_value("0.67")
                .help(
                    "minimum stake of the cluster which should receive \
                    a batch of shreds",
                ),
        )
        .arg(
            Arg::with_name("num_threads")
                .long("num-threads")
                .takes_value(true)
                .default_value("12")
                .help("number of ThreadPool worker threads"),
        )
        .get_matches();

    let batch_size = value_t_or_exit!(matches.value_of("batch_size"), usize);
    let config = Config {
        num_reps: value_t_or_exit!(matches.value_of("num_reps"), usize),
        node_index: value_t_or_exit!(matches.value_of("node_index"), usize),
        fanout: value_t_or_exit!(matches.value_of("fanout"), usize),
        batch_size,
        min_num_shards: value_t!(matches.value_of("min_num_shards"), usize)
            .unwrap_or(batch_size / 2),
        node_fault_rate: value_t_or_exit!(matches.value_of("node_fault_rate"), f64),
        edge_fault_rate: value_t_or_exit!(matches.value_of("edge_fault_rate"), f64),
        disable_parent_link: matches.is_present("disable_parent_link"),
        disable_first_node_link: matches.is_present("disable_first_node_link"),
        stake_threshold: value_t_or_exit!(matches.value_of("stake_threshold"), f64),
        num_threads: value_t_or_exit!(matches.value_of("num_threads"), usize),
    };
    let thread_pool = ThreadPoolBuilder::new()
        .num_threads(config.num_threads)
        .build()
        .unwrap();
    let rpc_client = RpcClient::new(API_MAINNET_BETA);
    let stakes = get_staked_nodes(&rpc_client);
    assert!(
        config.node_index < stakes.len(),
        "invalid node index: {}, cluster size: {}",
        config.node_index,
        stakes.len()
    );
    assert!((0.0..=1.0).contains(&config.node_fault_rate));
    assert!((0.0..=1.0).contains(&config.edge_fault_rate));
    assert!((0.0..=1.0).contains(&config.stake_threshold));
    assert!(config.stake_threshold + config.node_fault_rate <= 1.0);
    info!("config: {:#?}", config);
    let stakes = stakes.into_iter().map(|(_, stake)| stake).collect();
    let (rows, probs) = run_turbine(&config, &stakes, &thread_pool);
    println!(
        "* rows are number of shreds our node received \
        from a batch of size {}",
        config.batch_size
    );
    println!("* columns are number of shreds that the cluster received.");
    println!("* values are the probablity of this scenario.");
    print!("     ");
    let view = {
        let start = config.min_num_shards.saturating_sub(8);
        let end = config.batch_size.min(start + 16) + 1;
        start..end
    };
    for i in view.clone() {
        print!("{:3} ", i);
    }
    println!("");
    println!(
        "     {}",
        repeat("-")
            .take(view.clone().count() * 4)
            .collect::<String>()
    );
    for i in view.clone().next().unwrap()..config.batch_size + 1 {
        if rows[i] == 0.0 {
            break;
        }
        print!("{:3}: ", i);
        for j in view.clone() {
            print!("{:3.0} ", probs[i][j] * 100.0);
        }
        println!(" ({:3.0})", rows[i] * 100.0);
        if probs[i][config.min_num_shards] == 1.0 {
            break;
        }
    }
    println!(
        "prob[{}][{}]: {:.7}%",
        config.min_num_shards,
        config.min_num_shards,
        probs[config.min_num_shards][config.min_num_shards] * 100.0
    );
    for i in 0..config.batch_size + 1 {
        if probs[i][config.min_num_shards] >= 0.99999 {
            println!(
                "prob[{}][{}]: {:.7}%",
                i,
                config.min_num_shards,
                probs[i][config.min_num_shards] * 100.0
            );
            break;
        }
    }
}
