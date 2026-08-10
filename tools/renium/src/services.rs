pub(super) const DEFAULT_SYNC_SERVICES: [&str; 14] = [
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
];

pub(super) const EXTRA_EXPLORER_SERVICES: [&str; 4] = [
    "TextChatService",
    "TestService",
    "LocalizationService",
    "VRService",
];

pub(super) fn explorer_service_order(class_name: &str) -> Option<usize> {
    DEFAULT_SYNC_SERVICES
        .iter()
        .chain(EXTRA_EXPLORER_SERVICES.iter())
        .position(|value| *value == class_name)
}
