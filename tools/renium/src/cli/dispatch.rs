use std::path::Path;

use anyhow::Result;

use crate::app::setup::setup_command;
use crate::app::update;
use crate::automation::client::automation_command;
use crate::automation::tools::{
    asset_insert_command, asset_search_command, generate_model_command, http_get_command,
    image_store_command, job_status_command, script_grep_command, script_read_command,
    script_search_command,
};
use crate::bytecode::edit::{
    bytecode_add_instance, bytecode_clone_instance, bytecode_desync_package_link,
    bytecode_remove_instance,
};
use crate::bytecode::explorer::{
    bytecode_editor_targets, bytecode_explorer_batch, explorer_daemon,
};
use crate::bytecode::{
    bytecode_apply_property_batch, bytecode_get_property, bytecode_set_property,
    bytecode_set_source, find_command, inspect_command, tree_command,
};
use crate::cli::{Commands, ExecuteLuauArgs};
use crate::daemon::{bridge_daemon, bridge_get_source, cursor_poll};
use crate::editor::history::editor_revert;
use crate::editor::sync::{apply_editor_delete, apply_editor_property, push_editor_changes};
use crate::project::commands::{
    clone_instance_command, create_instance_command, desync_package_link_command,
    export_model_command, import_model_command, import_path_command, move_instance_command,
    remove_instance_command, rename_instance_command, syncback_command,
};
use crate::project::config;
use crate::project::package_links::place::place_desync_package_link;
use crate::project::package_links::{
    link_add, link_apply, link_break, link_delete_package, link_move_target, link_pack,
    link_status, sync_wally_packages,
};
use crate::project::sourcemap::generate_sourcemap_command;
use crate::project::version_control::{vc_init, vc_merge, vc_textconv, view_command};
use crate::project::workflows;
use crate::rbx::model::{
    bytecode_export_model, bytecode_export_place, bytecode_import_model, bytecode_repack,
};
use crate::snapshot::export::{export_snapshots, pull_from_studio};
use crate::snapshot::import::{import_service, import_snapshots};
use crate::studio::automation::{
    click_command, editor_review_decision_command, execute_luau_command,
    get_console_output_command, goto_command, key_command, list_clients_command, press_command,
    record_end_command, record_start_command, shot_command, start_stop_play_command,
    studio_change_state_command, studio_device_command, test_command, type_command, ui_command,
    wait_until_command,
};

pub(crate) fn dispatch(command: Commands, project: Option<&Path>) -> Result<()> {
    match command {
        Commands::Automation(args) => {
            automation_command(args);
            Ok(())
        }
        Commands::FmtProject(args) => config::run_fmt_project(args, project),
        Commands::ProjectValidate(args) => config::run_validate_project(args, project),
        Commands::ExplainPath(args) => config::run_explain_path(args, project),
        Commands::Config(args) => config::run_config(args),
        Commands::Adapters(args) => config::run_adapters(args, project),
        Commands::ImportRojo(args) => config::run_import_rojo(args),
        Commands::Init(args) => workflows::run_init(args),
        Commands::Build(args) => workflows::run_build(args, project),
        Commands::Doctor(args) => workflows::run_doctor(args, project),
        Commands::Docs(args) => workflows::run_docs(args),
        Commands::Daemon(args) => workflows::run_daemon(args),
        Commands::Studio(args) => workflows::run_studio(args, project),
        Commands::Upload(args) => workflows::run_upload(args, project),
        Commands::Update(args) => update::run_update(args),
        Commands::UpdateHelper(args) => update::run_update_helper(args),
        Commands::Syncback(args) => syncback_command(args, project),
        Commands::ImportPath(args) => import_path_command(args, project),
        Commands::Create(args) => create_instance_command(args, project),
        Commands::Clone(args) => clone_instance_command(args, project),
        Commands::Move(args) => move_instance_command(args, project),
        Commands::Rename(args) => rename_instance_command(args, project),
        Commands::Remove(args) => remove_instance_command(args, project),
        Commands::DesyncPackageLink(args) => desync_package_link_command(args, project),
        Commands::ImportModel(args) => import_model_command(args, project),
        Commands::ExportModel(args) => export_model_command(args, project),
        Commands::Test(args) => test_command(args),
        Commands::ExportSnapshots(args) => export_snapshots(args),
        Commands::Pull(args) => pull_from_studio(args),
        Commands::BridgeDaemon(args) => bridge_daemon(args),
        Commands::ExplorerDaemon(args) => explorer_daemon(args),
        Commands::BridgeGetSource(args) => bridge_get_source(args),
        Commands::GetConsoleOutput(args) => get_console_output_command(args),
        Commands::ExecuteLuau(args) => execute_luau_command(args),
        Commands::ExecuteClientLuau(args) => execute_luau_command(ExecuteLuauArgs {
            bridge: args.bridge,
            code: Some(args.code),
            inline_code: None,
            file: None,
            client: args.player.is_none(),
            player: args.player,
            timeout: args.timeout,
        }),
        Commands::StudioDevice(args) => studio_device_command(args),
        Commands::AssetSearch(args) => asset_search_command(args),
        Commands::AssetInsert(args) => asset_insert_command(args, project),
        Commands::GenerateModel(args) => generate_model_command(args, project),
        Commands::JobStatus(args) => job_status_command(args, project),
        Commands::ImageStore(args) => image_store_command(args, project),
        Commands::HttpGet(args) => http_get_command(args),
        Commands::ScriptSearch(args) => script_search_command(args, project),
        Commands::ScriptGrep(args) => script_grep_command(args, project),
        Commands::ScriptRead(args) => script_read_command(args, project),
        Commands::StartStopPlay(args) => start_stop_play_command(args),
        Commands::ListClients(args) => list_clients_command(args),
        Commands::EditorReviewDecision(args) => editor_review_decision_command(args),
        Commands::Press(args) => press_command(args),
        Commands::Click(args) => click_command(args),
        Commands::Key(args) => key_command(args),
        Commands::Ui(args) => ui_command(args),
        Commands::Type(args) => type_command(args),
        Commands::WaitUntil(args) => wait_until_command(args),
        Commands::Goto(args) => goto_command(args),
        Commands::Shot(args) => shot_command(args),
        Commands::RecordStart(args) => record_start_command(args),
        Commands::RecordEnd(args) => record_end_command(args),
        Commands::Setup(args) => setup_command(args),
        Commands::StudioChangeState(args) => studio_change_state_command(args),
        Commands::PushEditorChanges(args) => push_editor_changes(args),
        Commands::ApplyEditorProperty(args) => apply_editor_property(args),
        Commands::ApplyEditorDelete(args) => apply_editor_delete(args),
        Commands::EditorRevert(args) => editor_revert(args),
        Commands::Find(args) => find_command(args),
        Commands::Tree(args) => tree_command(args),
        Commands::Inspect(args) => inspect_command(args),
        Commands::BytecodeGetProperty(args) => bytecode_get_property(args),
        Commands::BytecodeSetProperty(args) => bytecode_set_property(args),
        Commands::BytecodeApplyPropertyBatch(args) => bytecode_apply_property_batch(args),
        Commands::BytecodeSetSource(args) => bytecode_set_source(args),
        Commands::BytecodeExplorerBatch(args) => bytecode_explorer_batch(args),
        Commands::BytecodeEditorTargets(args) => bytecode_editor_targets(args),
        Commands::BytecodeAddInstance(args) => bytecode_add_instance(args),
        Commands::BytecodeCloneInstance(args) => bytecode_clone_instance(args),
        Commands::BytecodeRemoveInstance(args) => bytecode_remove_instance(args),
        Commands::BytecodeDesyncPackageLink(args) => bytecode_desync_package_link(args),
        Commands::BytecodeExportModel(args) => bytecode_export_model(args),
        Commands::BytecodeExportPlace(args) => bytecode_export_place(args),
        Commands::PlaceDesyncPackageLink(args) => place_desync_package_link(args),
        Commands::BytecodeImportModel(args) => bytecode_import_model(args),
        Commands::SyncWallyPackages(args) => sync_wally_packages(args),
        Commands::LinkApply(args) => link_apply(args),
        Commands::LinkBreak(args) => link_break(args),
        Commands::LinkStatus(args) => link_status(args),
        Commands::LinkAdd(args) => link_add(args),
        Commands::LinkMoveTarget(args) => link_move_target(args),
        Commands::LinkPack(args) => link_pack(args),
        Commands::LinkDeletePackage(args) => link_delete_package(args),
        Commands::BytecodeRepack(args) => bytecode_repack(args),
        Commands::ImportSnapshots(args) => import_snapshots(args),
        Commands::ImportService(args) => import_service(args),
        Commands::GenerateSourcemap(args) => generate_sourcemap_command(args, project),
        Commands::VcInit(args) => vc_init(args),
        Commands::VcTextconv(args) => vc_textconv(args),
        Commands::View(args) => view_command(args),
        Commands::VcMerge(args) => vc_merge(args),
        Commands::CursorPoll(args) => cursor_poll(args),
    }
}
