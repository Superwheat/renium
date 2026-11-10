pub(crate) const DEFAULT_SYNC_SERVICES: [&str; 18] = [
    "Workspace",
    "Players",
    "Lighting",
    "MaterialService",
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer",
    "Teams",
    "SoundService",
    "VoiceChatService",
    "TextChatService",
    "TestService",
    "LocalizationService",
    "VRService",
];

pub(crate) fn explorer_service_order(class_name: &str) -> Option<usize> {
    DEFAULT_SYNC_SERVICES
        .iter()
        .position(|value| *value == class_name)
}

pub(crate) fn is_engine_managed_container(service: &str, class_name: &str) -> bool {
    matches!(
        (service, class_name),
        ("Workspace", "Terrain")
            | (
                "StarterPlayer",
                "StarterPlayerScripts" | "StarterCharacterScripts"
            )
            | (
                "TextChatService",
                "ChatWindowConfiguration"
                    | "ChatInputBarConfiguration"
                    | "BubbleChatConfiguration"
                    | "ChannelTabsConfiguration"
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbx::model::source_only_settings_document;
    use crate::snapshot::import::parse_services;

    #[test]
    fn added_services_accept_explicit_scope_and_keep_their_source_root_class() {
        let directory = crate::system::files::create_unique_directory(
            &std::env::temp_dir(),
            "renium-service-scope-",
        )
        .unwrap();
        let _cleanup = crate::system::files::OnDrop::new(|| {
            let _ = std::fs::remove_dir_all(&directory);
        });
        for service in [
            "TextChatService",
            "TestService",
            "LocalizationService",
            "VRService",
        ] {
            assert!(
                parse_services("")
                    .unwrap()
                    .iter()
                    .any(|name| name == service)
            );
            assert_eq!(
                parse_services(&format!(" {service},,{service} ")).unwrap(),
                [service]
            );
            let document = source_only_settings_document(&directory, service).unwrap();
            assert_eq!(document.instances[0].class_name, service);
            assert!(explorer_service_order(service).is_some());
        }
        for service in ["CoreGui", "ScriptContext", "Selection", "FilteredSelection"] {
            assert!(parse_services(service).is_err());
        }
    }
}
