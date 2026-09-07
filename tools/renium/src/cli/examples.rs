pub(super) const COMMAND_EXAMPLES: &[(&str, &str)] = &[
    (
        "access",
        "Examples:\n  rbx access mode\n  rbx access mode ask\n  rbx access mode read-only\n  rbx access mode read-write --accept-risk",
    ),
    (
        "perf",
        "Examples:\n  rbx perf snapshot --player 1\n  rbx perf start --player 1 --seconds 10\n  rbx perf stop --player 1 --capture ID\n  rbx perf export --player 1 --capture ID --out trace.json",
    ),
    (
        "net",
        "Examples:\n  rbx net presets\n  rbx net set --player 1 --preset mid\n  rbx net set --player 1 --in-delay 50 --out-jitter 10\n  rbx net show --player 2\n  rbx net restore --player 1",
    ),
    (
        "plugin",
        "Examples:\n  rbx plugin new my-workflow\n  rbx plugin install ./my-workflow\n  rbx plugin list\n  rbx my-workflow hello --name World",
    ),
    (
        "ck",
        "Examples:\n  rbx ck src/ServerScriptService/Main.server.luau src/ReplicatedStorage/Config.luau\n  Get-Content -Raw script.luau | rbx ck -",
    ),
    ("fmt", "Examples:\n  rbx fmt ."),
    ("pv", "Examples:\n  rbx pv"),
    ("xp", "Examples:\n  rbx xp src/Workspace/Door"),
    (
        "cfg",
        "Examples:\n  rbx cfg list\n  rbx cfg get liveSync.initialSyncPriority\n  rbx cfg set liveSync.initialSyncPriority reconcile",
    ),
    ("ad", "Examples:\n  rbx ad validate"),
    ("ir", "Examples:\n  rbx ir --project default.project.json"),
    ("init", "Examples:\n  rbx init ."),
    ("build", "Examples:\n  rbx build"),
    (
        "q",
        "Examples:\n  rbx q Place.rbxl -n Reward\n  rbx q Place.rbxl --source \"Free car\"",
    ),
    (
        "cmp",
        "Examples:\n  rbx cmp Place.rbxl\n  rbx cmp Before.rbxl --full --all\n  rbx cmp Before.rbxl --against After.rbxlx --full --all",
    ),
    ("dr", "Examples:\n  rbx dr"),
    ("docs", "Examples:\n  rbx docs sync"),
    ("dm", "Examples:\n  rbx dm list"),
    ("so", "Examples:\n  rbx so Place.rbxl"),
    ("ro", "Examples:\n  rbx ro Place.rbxl"),
    ("sx", "Examples:\n  rbx sx --save"),
    ("status", "Examples:\n  rbx status --all"),
    (
        "up",
        "Examples:\n  rbx up --place-id 123456 --universe-id 654321",
    ),
    ("upd", "Examples:\n  rbx upd"),
    (
        "oc",
        "Examples:\n  rbx oc key\n  rbx oc analytics metrics --field metric=DailyActiveUsers --field granularity=OneDay --field startTime=2026-01-01T00:00:00Z --field endTime=2026-02-01T00:00:00Z\n  rbx oc event list --limit 10\n  rbx oc experiment list --limit 25\n  rbx oc thumbnail personalization --limit 10",
    ),
    ("sb", "Examples:\n  rbx sb --dry-run"),
    (
        "ip",
        "Examples:\n  rbx ip car.rbxm --destination ServerStorage.Cars",
    ),
    (
        "cr",
        "Examples:\n  rbx cr Workspace --class-name Part --name Door",
    ),
    (
        "cp",
        "Examples:\n  rbx cp Workspace --settings-id editor:source --parent-settings-id editor:parent",
    ),
    (
        "mv",
        "Examples:\n  rbx mv Workspace -i editor:item -I editor:parent\n  rbx mv Workspace -i editor:item --to-service ReplicatedStorage",
    ),
    (
        "rn",
        "Examples:\n  rbx rn Workspace Door --settings-id editor:item",
    ),
    (
        "rm",
        "Examples:\n  rbx rm Workspace --settings-id editor:item",
    ),
    (
        "upl",
        "Examples:\n  rbx upl ReplicatedStorage --settings-id editor:package",
    ),
    ("pd", "Examples:\n  rbx pd ReplicatedStorage.testPackage"),
    ("pp", "Examples:\n  rbx pp ReplicatedStorage.testPackage"),
    ("pu", "Examples:\n  rbx pu ReplicatedStorage.testPackage"),
    (
        "mip",
        "Examples:\n  rbx mip ReplicatedStorage --parent-settings-id editor:parent --model car.rbxm",
    ),
    (
        "mep",
        "Examples:\n  rbx mep Workspace --settings-id editor:model --output model.rbxm",
    ),
    ("tst", "Examples:\n  rbx tst --mode play --timeout 30"),
    ("x", "Examples:\n  rbx x --snapshot-dir snapshots"),
    ("pl", "Examples:\n  rbx pl"),
    ("bd", "Examples:\n  rbx bd"),
    ("ed", "Examples:\n  rbx ed"),
    (
        "src",
        "Examples:\n  rbx src --service ServerScriptService --source-key editor:script",
    ),
    ("co", "Examples:\n  rbx co --player 1 --limit 20"),
    ("l", "Examples:\n  rbx l \"return game.PlaceId\""),
    ("lc", "Examples:\n  rbx lc \"return game.PlaceId\" 1"),
    ("dev", "Examples:\n  rbx dev set \"iPhone 16 Pro\""),
    (
        "pf",
        "Examples:\n  rbx pf ls\n  rbx pf use iphone-11\n  rbx pf show\n  rbx pf off\n  rbx pf adv cpu=25 cores=2 headroom=1g prio=low",
    ),
    ("as", "Examples:\n  rbx as \"sports car\" --limit 10"),
    ("ai", "Examples:\n  rbx ai 123456789"),
    ("gm", "Examples:\n  rbx gm \"small wooden cabin\""),
    ("js", "Examples:\n  rbx js job-id --wait-seconds 30"),
    ("is", "Examples:\n  rbx is icon.png"),
    ("iu", "Examples:\n  rbx iu icon.png --user 123456"),
    ("ss", "Examples:\n  rbx ss DataStore UpdateAsync"),
    ("sg", "Examples:\n  rbx sg \"DailyReward\""),
    (
        "sr",
        "Examples:\n  rbx sr src/ServerScriptService/Main.server.luau",
    ),
    ("play", "Examples:\n  rbx play\n  rbx play -x"),
    ("cs", "Examples:\n  rbx cs"),
    ("rv", "Examples:\n  rbx rv apply"),
    ("pr", "Examples:\n  rbx pr \"Shop.BuyButton\" -p 1"),
    ("clk", "Examples:\n  rbx clk 450 320 -p 1"),
    ("ky", "Examples:\n  rbx ky E -p 1"),
    ("ui", "Examples:\n  rbx ui -p 1"),
    (
        "ty",
        "Examples:\n  rbx ty \"hello\" --path \"Chat.Box\" --enter -p 1",
    ),
    (
        "wait",
        "Examples:\n  rbx wait \"workspace:GetAttribute('Ready') ~= nil\" -c -t 20",
    ),
    ("go", "Examples:\n  rbx go \"Workspace.Shop.Door\" -p 1"),
    ("sc", "Examples:\n  rbx sc --studio -o studio.png"),
    (
        "inp",
        "Examples:\n  rbx inp -p 1 click \"Shop.BuyButton\" wait 100 key E",
    ),
    ("rs", "Examples:\n  rbx rs -o playtest.mp4"),
    ("re", "Examples:\n  rbx re"),
    (
        "rf",
        "Examples:\n  rbx rf clip.mp4\n  rbx rf clip.mp4 --page 2\n  rbx rf clip.mp4 --frame 15",
    ),
    ("setup", "Examples:\n  rbx setup"),
    ("st", "Examples:\n  rbx st --event-wait-seconds 1"),
    (
        "lon",
        "Examples:\n  rbx lon\n  rbx lon --prefer studio\n  rbx lon --prefer editor",
    ),
    ("lof", "Examples:\n  rbx lof"),
    ("lst", "Examples:\n  rbx lst\n  rbx lst --wait 10"),
    ("rp", "Examples:\n  rbx rp"),
    ("dp", "Examples:\n  rbx dp"),
    ("ps", "Examples:\n  rbx ps src/StarterGui/Menu.client.luau"),
    (
        "prop",
        "Examples:\n  rbx prop --service Workspace --path-segments-json '[\"Door\"]' --property Name --value-json '\"Gate\"'",
    ),
    (
        "del",
        "Examples:\n  rbx del --service Workspace --path-segments-json '[\"TemporaryPart\"]'",
    ),
    (
        "rev",
        "Examples:\n  rbx rev --service Workspace --settings-id editor:item",
    ),
    (
        "me",
        "Examples:\n  rbx me src/ServerScriptService/Main.server.luau oldName newName",
    ),
    ("f", "Examples:\n  rbx f Workspace -n Door --limit 5"),
    ("tr", "Examples:\n  rbx tr Workspace Door --depth 2"),
    ("in", "Examples:\n  rbx in Workspace -i editor:item"),
    ("bg", "Examples:\n  rbx bg Workspace -i editor:item -p Name"),
    (
        "bs",
        "Examples:\n  rbx bs Workspace -i editor:item -p Name --str Gate",
    ),
    (
        "bss",
        "Examples:\n  rbx bss ServerScriptService -i editor:script --str \"print('hello')\"",
    ),
    (
        "bb",
        "Examples:\n  rbx bb Workspace -j '{\"ops\":[{\"type\":\"counts\"}]}'",
    ),
    ("bt", "Examples:\n  rbx bt --services Workspace,StarterGui"),
    (
        "ba",
        "Examples:\n  rbx ba Workspace -n Door -c Part -I editor:parent",
    ),
    (
        "bcl",
        "Examples:\n  rbx bcl Workspace -i editor:source -I editor:parent",
    ),
    ("br", "Examples:\n  rbx br Workspace -i editor:item"),
    (
        "bdp",
        "Examples:\n  rbx bdp ReplicatedStorage -i editor:package",
    ),
    (
        "bem",
        "Examples:\n  rbx bem Workspace -i editor:model -o model.rbxm",
    ),
    ("bep", "Examples:\n  rbx bep -o place.rbxl"),
    (
        "pdp",
        "Examples:\n  rbx pdp -i place.rbxl -o copy.rbxl --path '[\"Workspace\",\"Model\"]'",
    ),
    (
        "bim",
        "Examples:\n  rbx bim ReplicatedStorage --model model.rbxm -I editor:parent",
    ),
    ("wally", "Examples:\n  rbx wally"),
    ("lk", "Examples:\n  rbx lk"),
    ("lkb", "Examples:\n  rbx lkb --link shared-module"),
    ("lks", "Examples:\n  rbx lks"),
    (
        "lka",
        "Examples:\n  rbx lka --service ReplicatedStorage --path '[\"Shared\"]' --source src/Shared.luau",
    ),
    (
        "lkp",
        "Examples:\n  rbx lkp --service ReplicatedStorage --path '[\"Vehicles\",\"Car\"]'",
    ),
    ("lkd", "Examples:\n  rbx lkd --id shared-module"),
    (
        "bpack",
        "Examples:\n  rbx bpack Workspace ReplicatedStorage",
    ),
    (
        "si",
        "Examples:\n  rbx si --snapshot-dir snapshots --project-root .",
    ),
    (
        "ims",
        "Examples:\n  rbx ims --project-root . --service Workspace",
    ),
    ("sm", "Examples:\n  rbx sm -o sourcemap.json"),
    ("vci", "Examples:\n  rbx vci"),
    ("vct", "Examples:\n  rbx vct src/Workspace.renium"),
    ("v", "Examples:\n  rbx v model.rbxm --json"),
    (
        "vcm",
        "Examples:\n  rbx vcm base.renium ours.renium theirs.renium --output merged.renium",
    ),
    (
        "pa",
        "Examples:\n  rbx pa 123456 \"Lobby\" --game-id 654321 --alias lobby",
    ),
    ("pn", "Examples:\n  rbx pn 123456 main"),
    ("po", "Examples:\n  rbx po 123456 789012"),
];

#[cfg(test)]
mod tests {
    #[test]
    fn agent_guide_examples_use_canonical_commands() {
        use pulldown_cmark::{Event, Parser};
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut paths = vec![root.join("renium-agents.md")];
        paths.extend(
            std::fs::read_dir(root.join("renium-guides"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "md")),
        );
        let command = super::super::command();
        let mut examples = 0;
        for path in paths {
            let text = std::fs::read_to_string(&path).unwrap();
            for event in Parser::new(&text) {
                let (Event::Code(code) | Event::Text(code)) = event else {
                    continue;
                };
                for line in code.lines() {
                    let Some(example) = line.trim().strip_prefix("rbx ") else {
                        continue;
                    };
                    let mut tokens = example.split_whitespace();
                    while let Some(token) = tokens.next() {
                        if token == "<PLUGIN>" {
                            // Dynamic namespaces are discovered from installed manifests.
                            examples += 1;
                            break;
                        }
                        if let Some(option) = token.strip_prefix("--") {
                            let (name, inline_value) = option
                                .split_once('=')
                                .map_or((option, false), |(name, _)| (name, true));
                            let argument = command
                                .get_arguments()
                                .find(|arg| arg.get_long() == Some(name))
                                .unwrap_or_else(|| {
                                    panic!("{}: unknown global option {token}", path.display())
                                });
                            if argument.get_action().takes_values() && !inline_value {
                                tokens.next();
                            }
                            continue;
                        }
                        assert!(
                            command
                                .get_subcommands()
                                .any(|subcommand| subcommand.get_name() == token),
                            "{}: use a canonical short command, not {token}",
                            path.display()
                        );
                        examples += 1;
                        break;
                    }
                }
            }
        }
        assert!(examples > 0, "no command examples were checked");
    }
}
