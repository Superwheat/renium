use super::*;
use crate::editor::types::EditorInstanceDescriptor;

fn descriptor(id: &str, path: &[&str], previous: &[&str]) -> EditorInstanceDescriptor {
    EditorInstanceDescriptor {
        settings_id: id.into(),
        class_name: "Folder".into(),
        path_segments: path.iter().map(|segment| segment.to_string()).collect(),
        previous_path_segments: previous.iter().map(|segment| segment.to_string()).collect(),
        ..Default::default()
    }
}

fn change(mode: &str, instances: Vec<EditorInstanceDescriptor>) -> EditorInstanceChange {
    EditorInstanceChange {
        mode: mode.into(),
        service: "Workspace".into(),
        allow_deletes: false,
        instances,
        preserve_instances: Vec::new(),
    }
}

#[test]
fn instances_leaving_a_deleted_subtree_move_before_the_delete() {
    let mut changes = vec![
        change(
            "deleteInstances",
            vec![descriptor("holder", &["Workspace", "Lobby", "Holder"], &[])],
        ),
        change(
            "upsertInstances",
            vec![
                descriptor("lobby", &["Workspace", "Lobby"], &[]),
                descriptor(
                    "inside",
                    &["Workspace", "Lobby", "Inside"],
                    &["Workspace", "Lobby", "Holder", "Inside"],
                ),
                descriptor("other", &["Workspace", "Other"], &[]),
            ],
        ),
    ];
    move_escaping_instances_before_deletes(&mut changes);
    let modes = changes
        .iter()
        .map(|change| change.mode.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        modes,
        ["upsertInstances", "deleteInstances", "upsertInstances"]
    );
    let ids = |index: usize| {
        changes[index]
            .instances
            .iter()
            .map(|instance| instance.settings_id.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(0), ["lobby", "inside"]);
    assert_eq!(ids(2), ["other"]);
}
