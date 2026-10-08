use super::*;
use anyhow::ensure;

const TOP_ENTRIES: usize = 10;
const TOP_REFERENCES: usize = 15;
const PATHS_PER_REFERENCE: usize = 3;

/// Writes the Luau heap report a play runtime returned and summarises it:
/// the largest memory categories, tags and scripts, and the Instances that
/// left the DataModel but are still referenced from Luau, with the paths
/// that keep them alive.
pub(super) fn save_report(path: &Path, result: &Value) -> Result<Value> {
    let encoded = result["data"]
        .as_str()
        .context("Heap response omitted the report")?;
    let bytes = base64::decode(encoded).context("Invalid Luau heap report encoding")?;
    ensure!(
        result["bytes"].as_u64() == Some(bytes.len() as u64) && !bytes.is_empty(),
        "Luau heap report was incomplete; no file was written"
    );
    let report: Value = serde_json::from_slice(&bytes)
        .context("Studio returned a Luau heap report that is not JSON")?;
    let path = std::path::absolute(path)?;
    crate::system::files::atomic_write_file(&path, &bytes)?;
    Ok(json!({
        "ok": true,
        "path": path,
        "bytes": bytes.len(),
        "target": result["target"],
        "runtimeId": result["runtimeId"],
        "pid": result["pid"],
        "summary": summarize(&report),
        "hint": "unparentedInstanceReferences are Instances outside the DataModel that Luau still holds, with the paths keeping them alive; the file has the full per-script graph under Report.Graph and every root under Refs.Roots",
    }))
}

fn summarize(report: &Value) -> Value {
    let heap = &report["Report"];
    let refs = &report["Refs"];
    json!({
        "version": heap["Version"],
        "memoryCategories": top_stats(&heap["MemcatBreakdown"]),
        "objectTags": top_stats(&heap["TagBreakdown"]),
        "userdata": top_stats(&heap["UserdataBreakdown"]),
        "graphTotalSize": heap["Graph"]["TotalSize"],
        "largestScripts": largest_scripts(&heap["Graph"]),
        "rootReferences": refs["Roots"].as_array().map_or(0, Vec::len),
        "unparentedInstanceReferences": top_references(&refs["UnparentedReferences"]),
    })
}

fn sorted_by<'a>(entries: &'a Value, key: &str) -> Vec<&'a Value> {
    let mut entries: Vec<&Value> = entries.as_array().into_iter().flatten().collect();
    entries.sort_by_key(|entry| std::cmp::Reverse(entry[key].as_u64().unwrap_or(0)));
    entries
}

fn top_stats(entries: &Value) -> Vec<Value> {
    sorted_by(entries, "Size")
        .into_iter()
        .take(TOP_ENTRIES)
        .map(|entry| json!({"name": entry["Name"], "size": entry["Size"], "count": entry["Count"]}))
        .collect()
}

fn largest_scripts(graph: &Value) -> Vec<Value> {
    sorted_by(&graph["Children"], "TotalSize")
        .into_iter()
        .take(TOP_ENTRIES)
        .map(|entry| {
            let mut script = json!({"name": entry["Name"], "totalSize": entry["TotalSize"]});
            if entry["Source"].is_string() {
                script["source"] = entry["Source"].clone();
            }
            script
        })
        .collect()
}

fn top_references(entries: &Value) -> Vec<Value> {
    sorted_by(entries, "Instances")
        .into_iter()
        .take(TOP_REFERENCES)
        .map(|entry| {
            let paths: Vec<String> = entry["Paths"]
                .as_array()
                .into_iter()
                .flatten()
                .take(PATHS_PER_REFERENCE)
                .map(|path| {
                    path.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" > ")
                })
                .collect();
            json!({
                "name": entry["Name"],
                "count": entry["Count"],
                "instances": entry["Instances"],
                "paths": paths,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"{"Report":{"Version":1,
        "TagBreakdown":[{"Name":"Table","Size":100,"Count":3},{"Name":"Function","Size":900,"Count":2}],
        "MemcatBreakdown":[{"Name":"default","Size":10,"Count":1},{"Name":"Cars","Size":5000,"Count":40},{"Name":"UI","Size":200,"Count":9}],
        "UserdataBreakdown":[],
        "Graph":{"Name":"root","Size":1,"TotalSize":6000,"Children":[
            {"Name":"Small","Source":"ServerScriptService.Small","Size":1,"TotalSize":2,"Children":[]},
            {"Name":"RoundHandler","Source":"ServerScriptService.RoundHandler","Size":10,"TotalSize":4000,"Children":[]}]}},
        "Refs":{"Version":1,"Roots":[{"Name":"_G","Count":1,"Instances":0,"Paths":[]}],
        "UnparentedReferences":[
            {"Name":"Model","Count":1,"Instances":2,"Paths":[["RoundHandler","cars","1"]]},
            {"Name":"Player","Count":3,"Instances":3,"Paths":[["Telemetry","sessions","Player2"],["Telemetry","sessions","Player3"],["AC","tracked","Player2"],["extra"]]}]}}"#;

    #[test]
    fn summary_ranks_the_largest_entries_and_retained_instances() {
        let summary = summarize(&serde_json::from_str(REPORT).unwrap());
        assert_eq!(summary["memoryCategories"][0]["name"], "Cars");
        assert_eq!(summary["memoryCategories"][2]["name"], "default");
        assert_eq!(summary["objectTags"][0]["name"], "Function");
        assert_eq!(summary["largestScripts"][0]["name"], "RoundHandler");
        assert_eq!(
            summary["largestScripts"][0]["source"],
            "ServerScriptService.RoundHandler"
        );
        assert_eq!(summary["graphTotalSize"], 6000);
        assert_eq!(summary["rootReferences"], 1);
        let retained = &summary["unparentedInstanceReferences"];
        assert_eq!(retained[0]["name"], "Player");
        assert_eq!(retained[0]["instances"], 3);
        assert_eq!(retained[0]["paths"].as_array().unwrap().len(), 3);
        assert_eq!(retained[0]["paths"][0], "Telemetry > sessions > Player2");
        assert_eq!(retained[1]["paths"][0], "RoundHandler > cars > 1");
        assert!(summary["userdata"].as_array().unwrap().is_empty());
    }

    #[test]
    fn save_report_writes_the_decoded_json_and_refuses_short_data() {
        let directory = std::env::temp_dir().join(format!("renium-heap-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("heap.json");
        let result = json!({
            "data": base64::encode(REPORT.as_bytes()),
            "bytes": REPORT.len(),
            "target": "server",
            "runtimeId": "r1",
            "pid": 7,
        });
        let saved = save_report(&path, &result).unwrap();
        assert_eq!(saved["target"], "server");
        assert_eq!(saved["bytes"], REPORT.len());
        assert_eq!(
            saved["summary"]["unparentedInstanceReferences"][0]["name"],
            "Player"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), REPORT);
        let mut short = result.clone();
        short["bytes"] = json!(REPORT.len() - 1);
        assert!(save_report(&path, &short).is_err());
        let mut text = result;
        text["data"] = json!(base64::encode(b"not json"));
        text["bytes"] = json!(8);
        assert!(save_report(&path, &text).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }
}
