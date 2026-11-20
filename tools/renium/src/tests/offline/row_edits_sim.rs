//! Seeded runs of the edits that pick their instances by a query (`bs`, `rn`
//! and `br` with `--where`), each on two copies of one generated project: one
//! whose stores are of version 11, which those commands read whole, and one
//! whose stores are in small blocks, where a query reads the values it
//! compares by the rows of the store and the edit reads the store by its
//! instances. The first is the oracle, and the two are held to one another
//! after every command as `structure_edits_sim` holds them.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::structure_edits_sim::{
    CLASSES, NAMES, NEAR, SERVICES, fill, printed, projects, same_projects, tree,
};
use super::*;
use crate::tests::support::{SimRng, run_seeds};

const SEEDS: u64 = 32;
const STEPS: usize = 12;
const TAGS: [&str; 3] = ["alpha", "beta", "gamma"];
const TIERS: [&str; 3] = ["gold", "silver", "tin"];
const KINDS: [&str; 4] = [
    "set where",
    "rename where",
    "remove where",
    "query over the limit",
];
/// How often each kind of command went through, and how often a query was
/// refused for matching more instances than a call takes.
static DONE: [AtomicUsize; KINDS.len()] = [const { AtomicUsize::new(0) }; KINDS.len()];

/// Gives the instances the values the queries compare beside those `fill`
/// gave: tags, attributes, and a property that some parts hold and the
/// others take from their class.
fn compared_values(rng: &mut SimRng, stores: &mut BTreeMap<&str, SettingsBytecode>) {
    let instances = stores.values_mut().flat_map(|store| &mut store.instances);
    for instance in instances {
        if rng.below(3) == 0 {
            let tags = TAGS.iter().filter(|_| rng.below(2) == 0);
            let tags = json!(tags.collect::<Vec<_>>());
            instance.properties.insert("Tags".to_string(), tags);
        }
        if rng.below(3) == 0 {
            let weight = json!(rng.below(9) as f64);
            instance.attributes.insert("Weight".to_string(), weight);
        }
        if rng.below(4) == 0 {
            let tier = json!(TIERS[rng.below(TIERS.len())]);
            instance.attributes.insert("Tier".to_string(), tier);
        }
        if instance.class_name == "Part" && rng.below(2) == 0 {
            let transparency = json!(rng.below(3) as f64 / 2.0);
            let properties = &mut instance.properties;
            properties.insert("Transparency".to_string(), transparency);
        }
    }
}

/// One term of a query: a word of a name, a class, a tag, a comparison of a
/// property or an attribute in one token or in three, or a path.
fn term(rng: &mut SimRng) -> String {
    let name = NAMES[rng.below(NAMES.len())];
    let class = CLASSES[rng.below(CLASSES.len())];
    let number = rng.below(9);
    let operator = ["=", "!=", ">", "<", ">=", "<=", "==", "~="][rng.below(8)];
    match rng.below(15) {
        0 => name.to_lowercase(),
        1 => name[..2].to_string(),
        2 => format!("is:{class}"),
        3 => format!("tag:{}", TAGS[rng.below(TAGS.len())]),
        4 => format!("Weight{operator}{number}"),
        5 => format!("weight {operator} {number}"),
        6 => format!("Tier{operator}{}", TIERS[rng.below(TIERS.len())]),
        7 => format!("Transparency{operator}{}", rng.below(3) as f64 / 2.0),
        8 => format!("Anchored={}", rng.below(2) == 0),
        9 => format!("Blob={}", rng.below(4).to_string().repeat(3)),
        10 => format!("Label=label{}", rng.below(5)),
        11 => format!("{}={name}", ["Name", "Parent", "Value"][rng.below(3)]),
        12 => format!("ClassName={class}"),
        13 => "RunContext=Client".to_string(),
        _ => ["**", "*.Door", "Model.*", "Model.Part", "Part.**"][rng.below(5)].to_string(),
    }
}

fn query(rng: &mut SimRng) -> String {
    let mut terms = Vec::new();
    for _ in 0..1 + rng.below(2) {
        terms.push(term(rng));
    }
    if rng.below(3) == 0 {
        terms.extend(["or".to_string(), term(rng)]);
    }
    terms.join(" ")
}

/// One edit by a query of kind `kind`, under the whole service or under an
/// instance drawn from `document`, one of the first more often; `broad` asks
/// for a query most instances match.
fn query_edit(
    rng: &mut SimRng,
    kind: usize,
    (service, document): (&str, &SettingsBytecode),
    broad: bool,
) -> Vec<String> {
    let rows = document.instances.len();
    let scope = match rng.below(3) {
        0 => 0,
        1 => rng.below(rows.min(NEAR)),
        _ => rng.below(rows),
    };
    let scope = &document.instances[scope].settings_id;
    let scoped = rng.below(3) == 0;
    let variant = rng.below(6);
    let mark = format!("Mark={}", rng.below(9));
    let scripts = [
        "is:Script",
        "Code RunContext=Client",
        "RunContext!=Client is:Script",
    ];
    let query = match (kind, variant) {
        _ if broad => ["**", "Weight>=0 or o or a"][rng.below(2)].to_string(),
        (0, 0) => format!("is:Part {}", term(rng)),
        (0, 1) => scripts[rng.below(scripts.len())].to_string(),
        _ => query(rng),
    };
    let mut args = match kind {
        0 => vec!["bs", "-f", "<store>"],
        1 => vec!["rn", service, "-r", "<root>"],
        _ => vec!["br", "-f", "<store>"],
    };
    if scoped || kind == 1 {
        args.extend(["-i", scope]);
    }
    args.extend(["--where", &query]);
    match (kind, variant) {
        (0, 0) => args.extend(["-p", "Anchored=false"]),
        (0, 1) => args.extend(["-p", "RunContext=\"Client\""]),
        (0, 2) => args.extend(["-p", "Name=\"Renamed\""]),
        (0, 3) => args.extend(["-S", "attribute", "-p", &mark, "-p", "Tier=\"tin\""]),
        (0, _) => args.extend(["-S", "attribute", "-p", &mark]),
        (1, draw) => args.extend([
            "--replace",
            ["o", "Part", "el", "a", "D", "Code"][draw],
            ["x", "Zed", "o"][draw % 3],
        ]),
        (_, 0) => args.push("-R"),
        _ => {}
    }
    args.into_iter().map(str::to_string).collect()
}

fn run_seed(seed: u64, log: &mut Vec<String>) -> Result<(), String> {
    let mut rng = SimRng(seed ^ 0x9e_0e5);
    let block_rows = [2, 3, 5, 8, 13, 4096][rng.below(6)];
    // Now and then a store holds more instances than a query edit takes.
    let large = rng.below(4) == 0;
    let mut stores = BTreeMap::new();
    for service in SERVICES {
        let rows = match (large, service) {
            (true, "Workspace") => 520 + rng.below(80),
            _ => 12 + rng.below(50),
        };
        stores.insert(service, tree(&mut rng, service, rows));
    }
    fill(&mut rng, &mut stores);
    compared_values(&mut rng, &mut stores);
    let foreign = rng.below(5) == 0;
    let (oracle, sparse) = projects(&stores, block_rows, foreign)?;
    log.push(format!("blocks of {block_rows} rows, foreign: {foreign}"));
    // Half the scripts have a file, which a rename, a removal and another
    // run context of theirs reach.
    for (service, document) in &stores {
        for (row, instance) in document.instances.iter().enumerate() {
            if instance.class_name.ends_with("Script") && rng.below(2) == 0 {
                oracle.write_source(service, row, "return 1\n");
                sparse.write_source(service, row, "return 1\n");
            }
        }
    }
    for step in 0..STEPS {
        let stored = |service: &&str| oracle.store(service).is_file();
        let services = SERVICES.into_iter().filter(stored).collect::<Vec<_>>();
        let Some(service) = rng.pick(&services) else {
            break;
        };
        let document = oracle.read_store(service);
        let kind = rng.below(3);
        let broad = document.instances.len() > 500 && rng.below(2) == 0;
        let args = query_edit(&mut rng, kind, (service, &document), broad);
        log.push(format!("step {step}: {}", args.join(" ")));
        let run = |fixture: &Fixture| {
            let root = fixture.root.to_string_lossy().into_owned();
            let store = fixture.store_arg(service);
            let placed = |arg: &String| arg.replace("<root>", &root).replace("<store>", &store);
            let args = args.iter().map(placed).collect::<Vec<_>>();
            printed(fixture.json(&args.iter().map(String::as_str).collect::<Vec<_>>()))
        };
        let before = SERVICES.map(|service| fs::read(sparse.store(service)).ok());
        let (whole, blocks) = (run(&oracle), run(&sparse));
        if whole != blocks {
            return Err(format!(
                "step {step}: the whole stores gave {whole:?}, the stores in blocks {blocks:?}"
            ));
        }
        log.extend(whole.0.iter().map(|error| format!("  refused: {error}")));
        let done = match &whole.0 {
            None => Some(kind),
            Some(error) => error.contains("narrow the query").then_some(3),
        };
        if let Some(done) = done {
            DONE[done].fetch_add(1, Ordering::Relaxed);
        }
        same_projects(&oracle, &sparse, before).map_err(|error| format!("step {step}: {error}"))?;
    }
    Ok(())
}

#[test]
fn query_edits_of_stores_in_blocks_match_whole_documents() {
    run_seeds(SEEDS, run_seed);
    if std::env::var_os("RENIUM_SIM_SEED").is_some() {
        return;
    }
    for (name, done) in KINDS.into_iter().zip(&DONE) {
        assert!(done.load(Ordering::Relaxed) > 0, "no {name} went through");
    }
}
