"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.activate = activate;
exports.deactivate = deactivate;
const childProcess = require("child_process");
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const vscode = require("vscode");
const fileExplorer_1 = require("./fileExplorer");
const DEFAULT_SERVICES = [
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
];
const DEFAULT_BRIDGE_PORTS = [8781, 8782, 8783];
const PREVIOUS_DEFAULT_BRIDGE_PORTS = [8781, 8782, 8783, 8784];
const LEGACY_BRIDGE_PORTS = [8781, 8782, 8783, 8784, 8785, 8786, 8787, 8788];
const DEFAULT_CHUNK_SIZE = 4 * 1024 * 1024;
const TRANSIENT_SNAPSHOT_PROPERTY_NAMES = new Set([
    "absoluteposition",
    "absoluterotation",
    "absolutesize",
    "absolutecanvassize",
    "absolutewindowsize",
    "absolutecontentsize",
    "absolutecellcount",
    "absolutecellsize",
    "absolutepositionwrite",
    "absolutesizewrite",
    "arehingesdetected",
    "channelcount",
    "datamodelplaceversion",
    "floormaterial",
    "ispaused",
    "issmooth",
    "isspatial",
    "lastusedmodificationmethod",
    "localizedtext",
    "localizationmatchedsourcetext",
    "localizationmatchidentifier",
    "maxextents",
    "movedirection",
    "movedirectioninternal",
    "occupant",
    "opentypefeatureserror",
    "physicsreprrootpart",
    "rolloffgain",
    "rootpart",
    "seatpart",
    "steer",
    "terrain",
    "throttle",
    "timeposition",
    "timepositionreplicating",
    "timepositionreplicator",
    "resolution",
    "walkdirection",
    "weightcurrent",
    "weighttarget",
    "contenttext",
    "textbounds",
    "textfits",
    "assemblyangularvelocity",
    "assemblylinearvelocity",
    "assemblycenterofmass",
    "assemblymass",
    "assemblyrootpart",
    "centerofmass",
    "currentcamera",
    "currentphysicalproperties",
    "distributedgametime",
    "extentscframe",
    "extentssize",
    "isloaded",
    "isplaying",
    "mass",
    "networkissleeping",
    "playbackloudness",
    "receiveage",
    "rotvelocity",
    "timelength",
    "velocity",
]);
function sleep(ms) {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
function isTransientBridgeFailure(output) {
    return [
        "Bridge call failed",
        "Bridge send failed",
        "Bridge read failed",
        "Bridge closed while waiting",
        "closed before hello",
        "failed waiting for hello",
        "No plugin bridge channels connected",
        "Only ",
        "proceeding with",
    ].some((needle) => output.includes(needle));
}
class RobloxSyncController {
    constructor(context) {
        this.context = context;
        this.output = vscode.window.createOutputChannel("Renium");
        this.statusItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 200);
        this.queue = Promise.resolve();
        this.studioLiveSyncInFlight = false;
        this.studioLiveSyncStarted = false;
        this.studioToEditorImportInProgress = false;
        this.studioToEditorImportSuppressUntilMs = 0;
        this.studioToEditorLastSyncEndedAt = 0;
        this.studioSnapshotFingerprintByService = new Map();
        this.editorLiveSyncRuntimeEnabled = false;
        this.pendingEditorPaths = new Set();
        this.recentDirectSaveAtByPath = new Map();
        this.daemonRequestId = 1;
        this.daemonOutputBuffer = "";
        this.daemonReady = false;
        this.daemonPending = new Map();
        this.bridgeServeRequested = false;
        this.liveSyncOwnsServe = false;
        this.liveSyncStartupInProgress = false;
        this.liveSyncStopRequested = false;
        this.pendingAutoServices = new Set();
        this.activeTaskStartedAt = 0;
        this.warnedLegacyStartupWaitSeconds = false;
        this.warnedLegacyBridgePorts = false;
        this.warnedBridgePortLimit = false;
        this.warnedLegacyChunkSize = false;
        this.statusItem.command = "renium.openMenu";
        this.statusItem.show();
        this.updateStatusBar();
    }
    dispose() {
        if (this.autoSyncTimer) {
            clearTimeout(this.autoSyncTimer);
            this.autoSyncTimer = undefined;
        }
        if (this.activeTaskTicker) {
            clearInterval(this.activeTaskTicker);
            this.activeTaskTicker = undefined;
        }
        if (this.liveSyncTimer) {
            clearTimeout(this.liveSyncTimer);
            this.liveSyncTimer = undefined;
        }
        if (this.studioLiveSyncTimer) {
            clearTimeout(this.studioLiveSyncTimer);
            this.studioLiveSyncTimer = undefined;
        }
        if (this.liveSyncWatcher) {
            this.liveSyncWatcher.dispose();
            this.liveSyncWatcher = undefined;
        }
        this.stopBridgeDaemon();
        this.statusItem.dispose();
        this.output.dispose();
    }
    async openMenu() {
        const cfg = this.getConfig();
        const liveSyncRunning = cfg.editorLiveSyncEnabled || this.liveSyncWatcher !== undefined || this.liveSyncStartPromise !== undefined;
        const serving = this.bridgeServeRequested && this.isBridgeDaemonRunning();
        const items = [
            {
                label: "$(sync) Full Sync (Studio -> src)",
                description: "Exports from Studio, updates src, writes generated project JSON",
                action: "fullSync",
            },
            {
                label: "$(export) Export Snapshots Only",
                description: "Studio -> snapshots",
                action: "exportOnly",
            },
            {
                label: "$(file-code) Sync Active Service Now",
                description: "Fast service-targeted sync",
                action: "activeService",
            },
            {
                label: serving ? "$(debug-disconnect) Stop Serve" : "$(radio-tower) Serve",
                description: serving
                    ? `Stop bridge server on ${cfg.bridgePorts}`
                    : `Open bridge server on ${cfg.bridgePorts}; Studio plugin can connect once and reuse it`,
                action: serving ? "stopServe" : "serve",
            },
            {
                label: "$(arrow-down) Import Snapshots Into src",
                description: "Uses native Rust importer",
                action: "importOnly",
            },
            {
                label: liveSyncRunning ? "$(circle-slash) Stop Live Sync" : "$(broadcast) Live Sync",
                description: liveSyncRunning ? "Stop watching src and Studio changes" : "Two-way sync between src and Studio",
                action: liveSyncRunning ? "stopLive" : "startLive",
            },
            {
                label: cfg.autoSyncOnSave ? "$(circle-slash) Disable Auto Sync On Save" : "$(history) Enable Auto Sync On Save",
                description: `Debounce ${cfg.autoSyncDebounceMs}ms`,
                action: "toggleAuto",
            },
            {
                label: "$(output) Show Output",
                description: "Open extension logs",
                action: "showOutput",
            },
            {
                label: "$(pulse) Benchmark Full Sync",
                description: "Run full-sync timings with a warm-up and save metrics",
                action: "benchmarkFullSync",
            },
            {
                label: "$(beaker) Benchmark Modified-Default A/B",
                description: "Compare modified-default bypass OFF vs ON and save metrics",
                action: "benchmarkModifiedDefaultBypassAB",
            },
            {
                label: "$(dashboard) Profile Plugin Operations",
                description: "Run Studio-side operation timings and save raw JSON profile output",
                action: "profilePluginOps",
            },
        ];
        const picked = await vscode.window.showQuickPick(items, {
            title: "Renium",
            placeHolder: "Choose an action",
        });
        if (!picked) {
            return;
        }
        switch (picked.action) {
            case "fullSync":
                await this.fullSync();
                return;
            case "exportOnly":
                await this.exportSnapshotsOnly();
                return;
            case "importOnly":
                await this.importSnapshotsOnly();
                return;
            case "startLive":
                await this.startLiveSync();
                return;
            case "stopLive":
                await this.stopLiveSync();
                return;
            case "activeService":
                await this.syncActiveService();
                return;
            case "serve":
                await this.serve();
                return;
            case "stopServe":
                await this.stopServe();
                return;
            case "toggleAuto":
                await this.toggleAutoSyncOnSave();
                return;
            case "showOutput":
                this.output.show(true);
                return;
            case "benchmarkFullSync":
                await this.benchmarkFullSync();
                return;
            case "benchmarkModifiedDefaultBypassAB":
                await this.benchmarkModifiedDefaultBypassAB();
                return;
            case "profilePluginOps":
                await this.profilePluginOperations();
                return;
            default:
                return;
        }
    }
    async fullSync() {
        await this.enqueue("Full sync", async () => {
            await this.runExport({
                services: this.getConfig().services,
                runImport: this.getConfig().runImport,
                notifyOnSuccess: true,
                reason: "Full sync completed",
            });
        });
    }
    async exportSnapshotsOnly() {
        await this.enqueue("Export snapshots", async () => {
            await this.runExport({
                services: this.getConfig().services,
                runImport: false,
                notifyOnSuccess: true,
                reason: "Snapshot export completed",
            });
        });
    }
    async importSnapshotsOnly() {
        await this.enqueue("Import snapshots", async () => {
            const cfg = this.getConfig();
            const snapshotPath = this.resolveSnapshotPath(cfg);
            await this.runRustImport(cfg, snapshotPath, cfg.services);
            vscode.window.showInformationMessage("Renium: snapshot import finished.");
        });
    }
    async syncActiveService() {
        await this.enqueue("Sync active service", async () => {
            const cfg = this.getConfig();
            const activePath = vscode.window.activeTextEditor?.document.uri.fsPath;
            let service = activePath ? this.detectServiceForPath(activePath, cfg.projectRoot, cfg.services) : undefined;
            if (!service) {
                service = await vscode.window.showQuickPick(cfg.services, {
                    title: "Renium",
                    placeHolder: "Select a service to sync",
                });
            }
            if (!service) {
                return;
            }
            await this.runExport({
                services: [service],
                runImport: cfg.runImport,
                notifyOnSuccess: true,
                reason: `Synced ${service}`,
            });
        });
    }
    async serve(options = {}) {
        const cfg = this.getConfig();
        if (cfg.transport !== "ws") {
            const message = "Renium: serve requires WebSocket bridge transport.";
            if (options.bestEffort) {
                this.output.appendLine(`[renium] serve skipped: ${message}`);
                return;
            }
            throw new Error(message);
        }
        this.bridgeServeRequested = true;
        this.liveSyncOwnsServe = false;
        try {
            await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: true });
        }
        catch (err) {
            this.bridgeServeRequested = false;
            this.updateStatusBar();
            if (options.bestEffort) {
                this.output.appendLine(`[renium] serve failed: ${err instanceof Error ? err.message : String(err)}`);
                return;
            }
            throw err;
        }
        this.output.appendLine(`[renium] serve ready: plugin can connect on ${cfg.bridgePorts}`);
        this.updateStatusBar();
        if (!options.silent) {
            vscode.window.showInformationMessage(`Renium: serving bridge on ${cfg.bridgePorts}.`);
        }
    }
    async stopServe(options = {}) {
        this.bridgeServeRequested = false;
        this.liveSyncOwnsServe = false;
        this.stopBridgeDaemon();
        this.updateStatusBar();
        if (!options.silent) {
            vscode.window.showInformationMessage("Renium: serve stopped.");
        }
    }
    async benchmarkFullSync() {
        await this.enqueue("Benchmark full sync", async () => {
            const cfg = this.getConfig();
            const runCount = Math.max(1, cfg.benchmarkRuns);
            const runs = [];
            this.output.appendLine(`[renium] benchmark: running 1 warm-up + ${runCount} measured full sync iterations`);
            this.output.appendLine("[renium] benchmark: warm-up start (not counted)");
            const warmupResult = await this.runExport({
                services: cfg.services,
                runImport: cfg.runImport,
                notifyOnSuccess: false,
                reason: "",
                quietTimings: false,
            });
            const warmupMetrics = this.parseBenchmarkMetrics(warmupResult.output);
            this.logBenchmarkRun("[renium] benchmark: warm-up", warmupMetrics);
            for (let index = 0; index < runCount; index += 1) {
                this.output.appendLine(`[renium] benchmark: run ${index + 1}/${runCount} start`);
                const result = await this.runExport({
                    services: cfg.services,
                    runImport: cfg.runImport,
                    notifyOnSuccess: false,
                    reason: "",
                    quietTimings: false,
                });
                const metrics = this.parseBenchmarkMetrics(result.output);
                runs.push(metrics);
                this.logBenchmarkRun(`[renium] benchmark: run ${index + 1}/${runCount}`, metrics);
                if (metrics.exportFingerprint) {
                    this.output.appendLine(`[renium] benchmark: run ${index + 1}/${runCount} export=${metrics.exportFingerprint}`);
                }
                if (metrics.bridgeFingerprint) {
                    this.output.appendLine(`[renium] benchmark: run ${index + 1}/${runCount} bridge=${metrics.bridgeFingerprint}`);
                }
            }
            this.output.appendLine("[renium] benchmark summary:");
            this.output.appendLine(`[renium] benchmark: total ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.totalMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.totalMs), 0.9))} min=${this.formatMetricMs(this.minMetric(runs.map((run) => run.totalMs)))} max=${this.formatMetricMs(this.maxMetric(runs.map((run) => run.totalMs)))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service instance fetch ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceInstanceFetchMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceInstanceFetchMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service plugin server ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginServerMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginServerMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service plugin encode ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginEncodeMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServicePluginEncodeMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service payload bytes p50=${this.formatMetricBytes(this.percentile(runs.map((run) => run.trackedServicePayloadBytes), 0.5))} p90=${this.formatMetricBytes(this.percentile(runs.map((run) => run.trackedServicePayloadBytes), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service chunk count p50=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceChunkCount), 0.5))} p90=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceChunkCount), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service max frame ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceMaxFrameMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.trackedServiceMaxFrameMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service stall count >50ms p50=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver50Ms), 0.5))} p90=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver50Ms), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: tracked-service stall count >100ms p50=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver100Ms), 0.5))} p90=${this.formatMetricInt(this.percentile(runs.map((run) => run.trackedServiceStallCountOver100Ms), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: core export ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.coreExportMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.coreExportMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: bridge startup ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.bridgeStartupMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.bridgeStartupMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: handshake ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.handshakeMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.handshakeMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: service export sum ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.serviceExportSumMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.serviceExportSumMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: import tail ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.importCriticalTailMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.importCriticalTailMs), 0.9))}`);
            this.output.appendLine(`[renium] benchmark: unmeasured/scheduler gap ms p50=${this.formatMetricMs(this.percentile(runs.map((run) => run.unmeasuredOrSchedulerGapMs), 0.5))} p90=${this.formatMetricMs(this.percentile(runs.map((run) => run.unmeasuredOrSchedulerGapMs), 0.9))}`);
            const lastRun = runs[runs.length - 1];
            if (lastRun?.exportFingerprint) {
                this.output.appendLine(`[renium] benchmark: export fingerprint=${lastRun.exportFingerprint}`);
            }
            if (lastRun?.bridgeFingerprint) {
                this.output.appendLine(`[renium] benchmark: bridge fingerprint=${lastRun.bridgeFingerprint}`);
            }
            const benchmarkPath = path.join(cfg.projectRoot, ".renium", "benchmark-full-sync.latest.json");
            const benchmarkPayload = {
                generatedAt: new Date().toISOString(),
                runCount,
                services: cfg.services,
                runImport: cfg.runImport,
                importMode: cfg.importMode,
                performanceMode: cfg.performanceMode,
                modifiedDefaultBypass: cfg.modifiedDefaultBypass,
                chunkSize: cfg.chunkSize,
                bridgePorts: cfg.bridgePorts,
                warmup: warmupMetrics,
                summary: this.buildBenchmarkSummary(runs),
                runs: runs.map((metrics, index) => ({
                    index: index + 1,
                    ...metrics,
                })),
            };
            fs.mkdirSync(path.dirname(benchmarkPath), { recursive: true });
            fs.writeFileSync(benchmarkPath, JSON.stringify(benchmarkPayload, null, 2), "utf8");
            this.output.appendLine(`[renium] benchmark: saved metrics JSON to ${benchmarkPath}`);
            vscode.window.showInformationMessage(`Renium: benchmark full sync saved to ${benchmarkPath}.`);
        });
    }
    async benchmarkModifiedDefaultBypassAB() {
        await this.enqueue("Benchmark modified-default bypass A/B", async () => {
            const baseCfg = this.getConfig();
            const runCount = Math.max(1, baseCfg.benchmarkRuns);
            const variants = [
                { label: "off", modifiedDefaultBypass: false },
                { label: "on", modifiedDefaultBypass: true },
            ];
            const variantResults = [];
            this.output.appendLine(`[renium] benchmark-ab: running ${variants.length} variants, each with 1 warm-up + ${runCount} measured runs`);
            for (const variant of variants) {
                const cfg = {
                    ...baseCfg,
                    modifiedDefaultBypass: variant.modifiedDefaultBypass,
                };
                const runs = [];
                this.output.appendLine(`[renium] benchmark-ab: ${variant.label}: warm-up start (modifiedDefaultBypass=${variant.modifiedDefaultBypass}, not counted)`);
                const warmupResult = await this.runExport({
                    services: cfg.services,
                    runImport: cfg.runImport,
                    notifyOnSuccess: false,
                    reason: "",
                    quietTimings: false,
                    configOverrides: {
                        modifiedDefaultBypass: variant.modifiedDefaultBypass,
                    },
                });
                const warmup = this.parseBenchmarkMetrics(warmupResult.output);
                this.logBenchmarkRun(`[renium] benchmark-ab: ${variant.label}: warm-up`, warmup);
                for (let index = 0; index < runCount; index += 1) {
                    this.output.appendLine(`[renium] benchmark-ab: ${variant.label}: run ${index + 1}/${runCount} start`);
                    const result = await this.runExport({
                        services: cfg.services,
                        runImport: cfg.runImport,
                        notifyOnSuccess: false,
                        reason: "",
                        quietTimings: false,
                        configOverrides: {
                            modifiedDefaultBypass: variant.modifiedDefaultBypass,
                        },
                    });
                    const metrics = this.parseBenchmarkMetrics(result.output);
                    runs.push(metrics);
                    this.logBenchmarkRun(`[renium] benchmark-ab: ${variant.label}: run ${index + 1}/${runCount}`, metrics);
                }
                const summary = this.buildBenchmarkSummary(runs);
                variantResults.push({
                    label: variant.label,
                    modifiedDefaultBypass: variant.modifiedDefaultBypass,
                    warmup,
                    runs,
                    summary,
                });
            }
            const offSummary = variantResults.find((variant) => !variant.modifiedDefaultBypass)?.summary;
            const onSummary = variantResults.find((variant) => variant.modifiedDefaultBypass)?.summary;
            const offTotal = this.summaryP50(offSummary, "totalMs");
            const onTotal = this.summaryP50(onSummary, "totalMs");
            const offPlugin = this.summaryP50(offSummary, "trackedServicePluginServerMs");
            const onPlugin = this.summaryP50(onSummary, "trackedServicePluginServerMs");
            const totalDeltaMs = this.metricDelta(offTotal, onTotal);
            const pluginDeltaMs = this.metricDelta(offPlugin, onPlugin);
            this.output.appendLine(`[renium] benchmark-ab: total p50 off=${this.formatMetricMs(offTotal)} on=${this.formatMetricMs(onTotal)} delta=${this.formatSignedMetricMs(totalDeltaMs)}`);
            this.output.appendLine(`[renium] benchmark-ab: tracked-service plugin server p50 off=${this.formatMetricMs(offPlugin)} on=${this.formatMetricMs(onPlugin)} delta=${this.formatSignedMetricMs(pluginDeltaMs)}`);
            const benchmarkPath = path.join(baseCfg.projectRoot, ".renium", "benchmark-modified-default-bypass-ab.latest.json");
            const payload = {
                generatedAt: new Date().toISOString(),
                runCount,
                warmupRunsPerVariant: 1,
                services: baseCfg.services,
                runImport: baseCfg.runImport,
                importMode: baseCfg.importMode,
                performanceMode: baseCfg.performanceMode,
                chunkSize: baseCfg.chunkSize,
                bridgePorts: baseCfg.bridgePorts,
                comparison: {
                    totalP50DeltaMs: totalDeltaMs,
                    trackedServicePluginServerP50DeltaMs: pluginDeltaMs,
                    totalP50OffMs: offTotal,
                    totalP50OnMs: onTotal,
                    trackedServicePluginServerP50OffMs: offPlugin,
                    trackedServicePluginServerP50OnMs: onPlugin,
                },
                variants: variantResults.map((variant) => ({
                    label: variant.label,
                    modifiedDefaultBypass: variant.modifiedDefaultBypass,
                    warmup: variant.warmup,
                    summary: variant.summary,
                    runs: variant.runs.map((metrics, index) => ({
                        index: index + 1,
                        ...metrics,
                    })),
                })),
            };
            fs.mkdirSync(path.dirname(benchmarkPath), { recursive: true });
            fs.writeFileSync(benchmarkPath, JSON.stringify(payload, null, 2), "utf8");
            this.output.appendLine(`[renium] benchmark-ab: saved metrics JSON to ${benchmarkPath}`);
            vscode.window.showInformationMessage(`Renium: modified-default A/B benchmark saved to ${benchmarkPath}.`);
        });
    }
    async profilePluginOperations() {
        await this.enqueue("Profile plugin operations", async () => {
            const cfg = this.getConfig();
            const service = "ServerStorage";
            const sampleCount = 256;
            const iterations = 11;
            const flags = "luau,instance,serialize";
            const command = cfg.exportCliPath;
            const args = [
                "profile-plugin-ops",
                "--project-root",
                cfg.projectRoot,
                "--snapshot-dir",
                cfg.snapshotDir,
                "--service",
                service,
                "--transport",
                cfg.transport,
                "--source-workers",
                String(Math.max(0, cfg.sourceWorkers)),
                "--instance-workers",
                String(Math.max(0, cfg.instanceWorkers)),
                "--import-workers",
                String(Math.max(0, cfg.importWorkers)),
                "--performance-mode",
                cfg.performanceMode,
                ...(cfg.modifiedDefaultBypass ? ["--modified-default-bypass"] : ["--no-modified-default-bypass"]),
                "--chunk-size",
                String(Math.max(512, cfg.chunkSize)),
                "--snapshot-instance-chunk-size",
                String(Math.max(0, cfg.snapshotInstanceChunkSize)),
                "--bridge-wait-seconds",
                String(Math.max(1, cfg.bridgeWaitSeconds)),
                "--bridge-ports",
                cfg.bridgePorts,
                "--server",
                cfg.server,
                "--config",
                cfg.configTomlPath,
                "--ws-wait-seconds",
                String(Math.max(1, cfg.wsWaitSeconds)),
                cfg.adaptiveThrottle ? "--adaptive-throttle" : "--no-adaptive-throttle",
                cfg.noUpdateEditorIcons ? "--no-update-editor-icons" : "",
                "--sample-count",
                String(sampleCount),
                "--iterations",
                String(iterations),
                "--flags",
                flags,
            ].filter((value) => value.length > 0);
            this.output.show(false);
            this.logResolvedConfig(cfg);
            this.output.appendLine(`[renium] profile command: ${command} ${this.renderArgs(args)}`);
            const result = await this.runCommand(command, args, cfg.projectRoot, "profile-plugin-ops", cfg.progressHeartbeatSeconds);
            if (result.code !== 0) {
                throw new Error(`Plugin op profile exited with code ${result.code}`);
            }
            const profile = this.extractPluginProfile(result.output);
            const profilePath = path.join(cfg.projectRoot, ".renium", "profile-plugin-ops.latest.json");
            fs.mkdirSync(path.dirname(profilePath), { recursive: true });
            fs.writeFileSync(profilePath, JSON.stringify(profile, null, 2), "utf8");
            this.output.appendLine(`[renium] profile: saved raw JSON to ${profilePath}`);
            this.output.appendLine(`[renium] profile: ranked cost per 100k calls for ${service}`);
            for (const line of this.formatPluginProfileRanking(profile, 18)) {
                this.output.appendLine(line);
            }
            vscode.window.showInformationMessage(`Renium: plugin profile saved to ${profilePath}.`);
        });
    }
    async startLiveSync(options = {}) {
        if (this.liveSyncStartPromise) {
            await this.liveSyncStartPromise;
            return;
        }
        this.liveSyncStopRequested = false;
        const startPromise = this.startLiveSyncInternal(options);
        this.liveSyncStartPromise = startPromise;
        try {
            await startPromise;
        }
        finally {
            if (this.liveSyncStartPromise === startPromise) {
                this.liveSyncStartPromise = undefined;
            }
        }
    }
    async startLiveSyncInternal(options = {}) {
        this.liveSyncStartupInProgress = true;
        try {
            if (this.liveSyncWatcher) {
                await this.setEditorLiveSyncEnabled(true);
                const cfg = this.getConfig();
                if (this.liveSyncStopRequested) {
                    this.disposeLiveSyncRuntime();
                    await this.setEditorLiveSyncEnabled(false);
                    return;
                }
                if (cfg.studioLiveSyncEnabled && !this.studioLiveSyncStarted) {
                    if (!await this.ensureLiveSyncServeReady(cfg, options)) {
                        return;
                    }
                    if (this.liveSyncStopRequested) {
                        this.disposeLiveSyncRuntime();
                        await this.setEditorLiveSyncEnabled(false);
                        return;
                    }
                    await this.startStudioLiveSyncRuntime(cfg, options);
                }
                if (!options.silent) {
                    vscode.window.showInformationMessage("Renium: live sync is already running.");
                }
                return;
            }
            const cfg = this.getConfig();
            if (cfg.transport !== "ws") {
                if (!options.silent) {
                    vscode.window.showErrorMessage("Renium: editor -> Studio live sync requires WebSocket bridge transport.");
                }
                return;
            }
            try {
                this.ensureFileExists(cfg.exportCliPath);
            }
            catch (err) {
                if (!options.bestEffort) {
                    throw err;
                }
                const message = err instanceof Error ? err.message : String(err);
                this.output.appendLine(`[renium] editor live sync skipped: ${message}`);
                return;
            }
            const srcRoot = path.join(cfg.projectRoot, "src");
            if (!fs.existsSync(srcRoot)) {
                const message = `src directory not found: ${srcRoot}`;
                if (!options.bestEffort) {
                    throw new Error(message);
                }
                this.output.appendLine(`[renium] editor live sync skipped: ${message}`);
                return;
            }
            if (!await this.ensureLiveSyncServeReady(cfg, options)) {
                return;
            }
            if (this.liveSyncStopRequested) {
                return;
            }
            const watcher = vscode.workspace.createFileSystemWatcher(new vscode.RelativePattern(srcRoot, "**/*"));
            this.liveSyncWatcher = watcher;
            const queuePath = (uri) => {
                if (uri.scheme === "file") {
                    this.queueEditorChange(uri.fsPath);
                }
            };
            watcher.onDidCreate(queuePath);
            watcher.onDidChange(queuePath);
            watcher.onDidDelete(queuePath);
            await this.setEditorLiveSyncEnabled(true);
            if (this.liveSyncStopRequested) {
                this.disposeLiveSyncRuntime();
                await this.setEditorLiveSyncEnabled(false);
                return;
            }
            const liveCfg = this.getConfig();
            this.output.appendLine(`[renium] editor live sync watching: ${srcRoot}`);
            await this.runInitialEditorLiveSyncPass(srcRoot, options);
            if (this.liveSyncStopRequested) {
                this.disposeLiveSyncRuntime();
                await this.setEditorLiveSyncEnabled(false);
                return;
            }
            await this.startStudioLiveSyncRuntime(liveCfg, options);
            this.updateStatusBar();
            if (!options.silent) {
                vscode.window.showInformationMessage("Renium: editor -> Studio live sync started.");
            }
        }
        catch (err) {
            this.disposeLiveSyncRuntime();
            await this.setEditorLiveSyncEnabled(false);
            throw err;
        }
        finally {
            this.liveSyncStartupInProgress = false;
        }
    }
    async ensureLiveSyncServeReady(cfg, options = {}) {
        if (cfg.transport !== "ws") {
            return true;
        }
        const startedServe = !this.bridgeServeRequested;
        this.bridgeServeRequested = true;
        if (startedServe) {
            this.liveSyncOwnsServe = true;
        }
        try {
            await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: true });
            const result = await this.runDaemonCommand(cfg.exportCliPath, [], cfg, "live-sync-wait-for-plugin", "wait-for-channels", { quietWait: true });
            if (result.code !== 0) {
                throw new Error("Studio plugin bridge did not connect.");
            }
            return true;
        }
        catch (err) {
            if (startedServe) {
                this.bridgeServeRequested = false;
                this.liveSyncOwnsServe = false;
                this.stopBridgeDaemon();
            }
            if (!options.bestEffort) {
                throw err;
            }
            this.output.appendLine(`[renium] editor live sync waiting for Studio plugin failed: ${err instanceof Error ? err.message : String(err)}`);
            return false;
        }
    }
    async runInitialEditorLiveSyncPass(srcRoot, options = {}) {
        const initialPaths = this.collectInitialEditorLiveSyncSettingsPaths(srcRoot);
        const initialTargets = this.collectInitialEditorLiveSyncTargetIds(srcRoot, initialPaths);
        this.output.appendLine(`[renium] editor live sync initial targeted sync: ${initialTargets.paths.length} settings file(s), ${initialTargets.targetSettingsIds.length} editor instance(s)`);
        if (initialTargets.paths.length === 0 || initialTargets.targetSettingsIds.length === 0) {
            this.primeEditorLiveSyncCache([], this.getConfig());
            return;
        }
        try {
            await this.pushEditorPathsNow(initialTargets.paths, {
                force: true,
                skipChangeFilter: true,
                targetSettingsIds: initialTargets.targetSettingsIds,
                taskName: "Editor -> Studio initial sync",
            });
        }
        catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            this.output.appendLine(`[renium] editor live sync initial pass failed: ${message}`);
            if (!options.bestEffort) {
                throw err;
            }
        }
    }
    async retryEditorInitialSync() {
        const cfg = this.getConfig();
        const srcRoot = path.join(cfg.projectRoot, "src");
        if (!fs.existsSync(srcRoot)) {
            throw new Error(`src directory not found: ${srcRoot}`);
        }
        await this.runInitialEditorLiveSyncPass(srcRoot);
        vscode.window.showInformationMessage("Renium: editor -> Studio initial sync finished.");
    }
    async startStudioLiveSyncRuntime(cfg, options = {}) {
        if (!cfg.studioLiveSyncEnabled) {
            this.stopStudioLiveSyncRuntime();
            return;
        }
        try {
            if (this.studioLiveSyncStarted) {
                this.scheduleStudioLiveSyncPoll(cfg, cfg.studioLiveSyncPollMs);
                return;
            }
            await this.getStudioChangeState(cfg, cfg.services, { reset: true, start: true });
            await this.enqueue("Studio -> Editor initial sync", async () => {
                await this.runStudioToEditorSync(cfg.services, cfg);
            });
            await this.getStudioChangeState(cfg, cfg.services, { reset: true, start: true });
            this.studioLiveSyncStarted = true;
            this.scheduleStudioLiveSyncPoll(cfg, cfg.studioLiveSyncPollMs);
        }
        catch (err) {
            this.stopStudioLiveSyncRuntime();
            const message = err instanceof Error ? err.message : String(err);
            this.output.appendLine(`[renium] Studio -> editor live sync start failed: ${message}`);
            if (!options.bestEffort) {
                throw err;
            }
        }
    }
    stopStudioLiveSyncRuntime() {
        if (this.studioLiveSyncTimer) {
            clearTimeout(this.studioLiveSyncTimer);
            this.studioLiveSyncTimer = undefined;
        }
        this.studioLiveSyncInFlight = false;
        this.studioLiveSyncStarted = false;
        this.studioToEditorImportInProgress = false;
    }
    scheduleStudioLiveSyncPoll(cfg, delayMs) {
        if (this.studioLiveSyncTimer) {
            clearTimeout(this.studioLiveSyncTimer);
            this.studioLiveSyncTimer = undefined;
        }
        if (!cfg.editorLiveSyncEnabled || !this.liveSyncWatcher || !cfg.studioLiveSyncEnabled) {
            return;
        }
        this.studioLiveSyncTimer = setTimeout(() => {
            this.studioLiveSyncTimer = undefined;
            void this.pollStudioLiveSync().catch((err) => {
                const message = err instanceof Error ? err.message : String(err);
                this.output.appendLine(`[renium] Studio -> editor live sync failed: ${message}`);
                this.scheduleStudioLiveSyncPoll(this.getConfig(), this.getConfig().studioLiveSyncPollMs);
            });
        }, Math.max(250, delayMs));
    }
    async pollStudioLiveSync() {
        const cfg = this.getConfig();
        if (!cfg.editorLiveSyncEnabled || !this.liveSyncWatcher || !cfg.studioLiveSyncEnabled) {
            this.stopStudioLiveSyncRuntime();
            return;
        }
        if (this.studioLiveSyncInFlight) {
            this.scheduleStudioLiveSyncPoll(cfg, cfg.studioLiveSyncPollMs);
            return;
        }
        this.studioLiveSyncInFlight = true;
        try {
            const state = await this.getStudioChangeState(cfg, cfg.services, { start: true });
            const dirtyServices = Array.isArray(state.dirtyServices)
                ? this.normalizeServices(state.dirtyServices, cfg.services)
                : [];
            const observedSeq = this.studioChangeSeq(state);
            if (dirtyServices.length > 0) {
                const ackObservedDirty = this.studioChangeAckOptions(observedSeq);
                if (this.shouldDropLikelySelfDirtyStudioState(dirtyServices, cfg)) {
                    ackObservedDirty.suppressSeconds = Math.max(1, Math.min(4, cfg.studioLiveSyncPollMs / 1000 + 1.5));
                    await this.getStudioChangeState(cfg, dirtyServices, ackObservedDirty);
                    return;
                }
                await this.enqueueStudioToEditorSyncIfChanged(dirtyServices, cfg);
                await this.getStudioChangeState(cfg, dirtyServices, ackObservedDirty);
            }
        }
        finally {
            this.studioLiveSyncInFlight = false;
            this.scheduleStudioLiveSyncPoll(this.getConfig(), this.getConfig().studioLiveSyncPollMs);
        }
    }
    async getStudioChangeState(cfg, services, options = {}) {
        const command = cfg.exportCliPath;
        this.ensureFileExists(command);
        const args = [
            "-w",
            String(this.editorBridgeWaitSeconds(cfg)),
            "-P",
            cfg.bridgePorts,
            "-s",
            this.normalizeServices(services, cfg.services).join(","),
        ];
        if (options.reset === true) {
            args.push("--reset");
        }
        if (options.start === false) {
            args.push("--no-start");
        }
        if (typeof options.ackSeq === "number" && Number.isFinite(options.ackSeq)) {
            args.push("--ack-seq", String(Math.max(0, Math.floor(options.ackSeq))));
        }
        if (typeof options.suppressSeconds === "number" && Number.isFinite(options.suppressSeconds) && options.suppressSeconds > 0) {
            args.push("--suppress-seconds", String(Math.max(0.05, options.suppressSeconds)));
        }
        const result = await this.runDaemonCommand(command, args, cfg, "studio-change-state", "st", { quietWait: true });
        if (result.code !== 0) {
            throw new Error(`Studio change state exited with code ${result.code}`);
        }
        const state = this.parseStudioChangeState(result.output);
        if (!state) {
            throw new Error("Studio change state did not return a plugin result.");
        }
        return state;
    }
    studioChangeSeq(state) {
        if (typeof state.seq !== "number" || !Number.isFinite(state.seq)) {
            return undefined;
        }
        return Math.max(0, Math.floor(state.seq));
    }
    studioChangeAckOptions(observedSeq) {
        const options = { start: true };
        if (observedSeq !== undefined) {
            options.ackSeq = observedSeq;
        }
        else {
            options.reset = true;
        }
        return options;
    }
    async runStudioToEditorSync(services, cfg) {
        const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg);
        if (diff.changedServices.length === 0) {
            return;
        }
        await this.importStudioLiveSyncSnapshot(diff.changedServices, cfg, diff.fingerprintsByService);
    }
    async enqueueStudioToEditorSyncIfChanged(services, cfg) {
        const run = async () => {
            let taskStarted = false;
            const taskName = "Studio -> Editor sync";
            try {
                const diff = await this.exportStudioLiveSyncSnapshotAndDiff(services, cfg, { quietProbe: true });
                if (diff.changedServices.length === 0) {
                    return;
                }
                taskStarted = true;
                this.setActiveTask(taskName);
                this.output.appendLine(`[renium] task start: ${taskName}`);
                await this.importStudioLiveSyncSnapshot(diff.changedServices, cfg, diff.fingerprintsByService);
                this.output.appendLine(`[renium] task done: ${taskName}`);
            }
            catch (err) {
                const message = err instanceof Error ? err.message : String(err);
                if (taskStarted) {
                    this.output.appendLine(`[renium] task failed: ${taskName}: ${message}`);
                    this.output.show(true);
                    vscode.window.showErrorMessage(`Renium: ${taskName} failed. ${message}`);
                }
                else {
                    this.output.appendLine(`[renium] Studio -> editor dirty check failed: ${message}`);
                }
                throw err;
            }
            finally {
                if (taskStarted) {
                    this.setActiveTask(undefined);
                }
            }
        };
        this.queue = this.queue.then(run, run);
        await this.queue;
    }
    async exportStudioLiveSyncSnapshotAndDiff(services, cfg, options = {}) {
        const selectedServices = this.normalizeServices(services, cfg.services);
        await this.getStudioChangeState(cfg, selectedServices, { start: true });
        await this.runExport({
            services: selectedServices,
            runImport: false,
            notifyOnSuccess: false,
            reason: "",
            quietLog: options.quietProbe === true,
        });
        return this.diffServicesBySnapshotFingerprint(selectedServices, cfg);
    }
    async importStudioLiveSyncSnapshot(services, cfg, fingerprintsByService) {
        const selectedServices = this.normalizeServices(services, cfg.services);
        this.studioToEditorImportInProgress = true;
        try {
            await this.runRustImport(cfg, this.resolveSnapshotPath(cfg), selectedServices);
            this.commitStudioSnapshotFingerprints(selectedServices, fingerprintsByService);
            this.replaceEditorLiveSyncCacheForServices(selectedServices, cfg);
            try {
                await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", selectedServices);
            }
            catch {

            }
        }
        finally {
            this.studioToEditorImportSuppressUntilMs = Date.now() + Math.max(1000, Math.min(3000, cfg.studioLiveSyncPollMs * 2));
            this.studioToEditorImportInProgress = false;
            this.studioToEditorLastSyncEndedAt = Date.now();
        }
    }
    shouldDropLikelySelfDirtyStudioState(dirtyServices, cfg) {
        if (dirtyServices.length < cfg.services.length) {
            return false;
        }
        return Date.now() - this.studioToEditorLastSyncEndedAt < 10000;
    }
    diffServicesBySnapshotFingerprint(services, cfg) {
        const changedServices = [];
        const fingerprintsByService = new Map();
        for (const service of services) {
            const fingerprint = this.snapshotFingerprintForService(service, cfg);
            if (!fingerprint) {
                changedServices.push(service);
                continue;
            }
            fingerprintsByService.set(service, fingerprint);
            const previous = this.studioSnapshotFingerprintByService.get(service);
            if (previous !== fingerprint) {
                changedServices.push(service);
            }
        }
        if (changedServices.length === 0) {
            this.output.appendLine(`[renium] Studio -> editor dirty state ignored: exported snapshot unchanged for ${services.length} service(s)`);
        }
        return { changedServices, fingerprintsByService };
    }
    commitStudioSnapshotFingerprints(services, fingerprintsByService) {
        if (!fingerprintsByService) {
            return;
        }
        for (const service of services) {
            const fingerprint = fingerprintsByService.get(service);
            if (fingerprint) {
                this.studioSnapshotFingerprintByService.set(service, fingerprint);
            }
        }
    }
    snapshotFingerprintForService(service, cfg) {
        const snapshotRoot = this.resolveSnapshotPath(cfg);
        const paths = this.collectSnapshotFingerprintPaths(snapshotRoot, service);
        if (paths.length === 0) {
            return undefined;
        }
        const rootFile = path.join(snapshotRoot, service + ".json");
        const hash = crypto.createHash("sha256");
        let hashedAnyFile = false;
        for (const filePath of paths) {
            let stat;
            try {
                stat = fs.statSync(filePath);
            }
            catch {
                continue;
            }
            if (!stat.isFile()) {
                continue;
            }
            const relPath = this.normalizePathForCompare(path.relative(snapshotRoot, filePath));
            const content = fs.readFileSync(filePath);
            const fingerprintContent = path.resolve(filePath) === path.resolve(rootFile)
                ? this.normalizeSnapshotRootForFingerprint(content, service)
                : content;
            hash.update(relPath);
            hash.update("\0");
            hash.update(String(fingerprintContent.length));
            hash.update("\0");
            hash.update(fingerprintContent);
            hash.update("\0");
            hashedAnyFile = true;
        }
        return hashedAnyFile ? hash.digest("hex") : undefined;
    }
    normalizeSnapshotRootForFingerprint(content, service) {
        const text = content.toString("utf8");
        try {
            const parsed = JSON.parse(text);
            if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
                const snapshot = parsed;
                const filteredInstanceCount = this.normalizeSnapshotInstancesForFingerprint(snapshot, service);
                const metadata = snapshot.metadata;
                if (metadata && typeof metadata === "object" && !Array.isArray(metadata)) {
                    const stableMetadata = { ...metadata };
                    delete stableMetadata.generatedAtUnix;
                    if (filteredInstanceCount !== undefined) {
                        stableMetadata.instanceCount = filteredInstanceCount;
                    }
                    snapshot.metadata = stableMetadata;
                }
                return Buffer.from(this.stableJsonStringify(snapshot), "utf8");
            }
        }
        catch {

        }
        return Buffer.from(text.replace(/(\"generatedAtUnix\"\s*:\s*)-?\d+(\s*,?)/g, (_match, prefix, suffix) => prefix + "0" + suffix), "utf8");
    }
    normalizeSnapshotInstancesForFingerprint(snapshot, service) {
        const rawInstances = snapshot.instances;
        if (!Array.isArray(rawInstances)) {
            return undefined;
        }
        const entries = rawInstances.map((entry) => (entry && typeof entry === "object" && !Array.isArray(entry)
            ? { ...entry }
            : entry));
        const removedIndices = new Set();
        let changed = false;
        for (let index = 0; index < entries.length; index += 1) {
            const entry = entries[index];
            if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
                continue;
            }
            const instance = entry;
            if (this.normalizeSnapshotPropertiesForFingerprint(instance)) {
                changed = true;
            }
            if (service === "Workspace" && index === 0) {
                const properties = instance.properties;
                if (properties && typeof properties === "object" && !Array.isArray(properties) && "CurrentCamera" in properties) {
                    const stableProperties = { ...properties };
                    delete stableProperties.CurrentCamera;
                    instance.properties = stableProperties;
                    changed = true;
                }
            }
            if (instance.className === "Camera") {
                removedIndices.add(this.snapshotInstanceIndex(instance, index));
                changed = true;
            }
        }
        const filtered = entries.filter((entry, index) => {
            if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
                return true;
            }
            return !removedIndices.has(this.snapshotInstanceIndex(entry, index));
        });
        if (changed || filtered.length !== rawInstances.length) {
            snapshot.instances = filtered;
        }
        return filtered.length;
    }
    normalizeSnapshotPropertiesForFingerprint(instance) {
        const properties = instance.properties;
        if (!properties || typeof properties !== "object" || Array.isArray(properties)) {
            return false;
        }
        const source = properties;
        let stableProperties;
        for (const key of Object.keys(source)) {
            if (TRANSIENT_SNAPSHOT_PROPERTY_NAMES.has(key.toLowerCase())) {
                if (!stableProperties) {
                    stableProperties = { ...source };
                }
                delete stableProperties[key];
            }
        }
        if (!stableProperties) {
            return false;
        }
        instance.properties = stableProperties;
        return true;
    }
    snapshotInstanceIndex(instance, fallbackIndex) {
        return this.snapshotNumericIndex(instance.instanceIndex) ?? fallbackIndex + 1;
    }
    snapshotNumericIndex(value) {
        if (typeof value !== "number" || !Number.isFinite(value)) {
            return undefined;
        }
        const index = Math.floor(value);
        return index > 0 ? index : undefined;
    }
    stableJsonStringify(value) {
        if (Array.isArray(value)) {
            return "[" + value.map((entry) => this.stableJsonStringify(entry)).join(",") + "]";
        }
        if (value && typeof value === "object") {
            const record = value;
            return "{" + Object.keys(record)
                .sort()
                .map((key) => JSON.stringify(key) + ":" + this.stableJsonStringify(record[key]))
                .join(",") + "}";
        }
        const primitive = JSON.stringify(value);
        return primitive === undefined ? "null" : primitive;
    }
    collectSnapshotFingerprintPaths(snapshotRoot, service) {
        const paths = [];
        const rootFile = path.join(snapshotRoot, `${service}.json`);
        if (fs.existsSync(rootFile)) {
            paths.push(rootFile);
        }
        const rootDir = path.join(snapshotRoot, service);
        if (fs.existsSync(rootDir)) {
            const stack = [rootDir];
            while (stack.length > 0) {
                const dir = stack.pop();
                if (!dir) {
                    continue;
                }
                let entries;
                try {
                    entries = fs.readdirSync(dir, { withFileTypes: true });
                }
                catch {
                    continue;
                }
                for (const entry of entries) {
                    const fullPath = path.join(dir, entry.name);
                    if (entry.isDirectory()) {
                        stack.push(fullPath);
                    }
                    else if (entry.isFile()) {
                        paths.push(fullPath);
                    }
                }
            }
        }
        return paths.sort((a, b) => this.normalizePathForCompare(a).localeCompare(this.normalizePathForCompare(b)));
    }
    collectInitialEditorLiveSyncPaths(srcRoot) {
        const settingsPaths = [];
        const otherPaths = [];
        const stack = [srcRoot];
        while (stack.length > 0) {
            const dir = stack.pop();
            if (!dir) {
                continue;
            }
            let entries;
            try {
                entries = fs.readdirSync(dir, { withFileTypes: true });
            }
            catch {
                continue;
            }
            for (const entry of entries) {
                const fullPath = path.join(dir, entry.name);
                if (entry.isDirectory()) {
                    stack.push(fullPath);
                    continue;
                }
                if (!entry.isFile()) {
                    continue;
                }
                if (entry.name.toLowerCase() === "__roblox_sync_settings.rbsync") {
                    settingsPaths.push(fullPath);
                }
                else {
                    otherPaths.push(fullPath);
                }
            }
        }
        return [
            ...settingsPaths.sort((a, b) => a.localeCompare(b)),
            ...otherPaths.sort((a, b) => a.localeCompare(b)),
        ];
    }
    collectInitialEditorLiveSyncSettingsPaths(srcRoot) {
        return this.collectInitialEditorLiveSyncPaths(srcRoot)
            .filter((filePath) => path.basename(filePath).toLowerCase() === "__roblox_sync_settings.rbsync");
    }
    collectEditorLiveSyncPathsForServices(services, cfg) {
        const srcRoot = path.join(cfg.projectRoot, "src");
        const selectedServices = this.normalizeServices(services, cfg.services);
        const paths = [];
        for (const service of selectedServices) {
            const serviceDir = path.join(srcRoot, service);
            if (!fs.existsSync(serviceDir)) {
                continue;
            }
            paths.push(...this.collectInitialEditorLiveSyncPaths(serviceDir));
        }
        return [...new Set(paths.map((filePath) => path.resolve(filePath)))].sort((a, b) => a.localeCompare(b));
    }
    collectInitialEditorLiveSyncTargetIds(srcRoot, settingsPaths) {
        const cfg = this.getConfig();
        const result = childProcess.spawnSync(cfg.exportCliPath, [
            "bt",
            "-d",
            srcRoot,
            "-s",
            cfg.services.join(","),
        ], {
            cwd: cfg.projectRoot,
            encoding: "utf8",
            maxBuffer: 16 * 1024 * 1024,
            windowsHide: true,
        });
        if (result.status !== 0) {
            const message = (result.stderr || result.stdout || "").trim();
            this.output.appendLine(`[renium] editor live sync initial target scan failed: ${message || `exit ${result.status}`}`);
            return { paths: [], targetSettingsIds: [] };
        }
        let parsed;
        try {
            parsed = JSON.parse(result.stdout);
        }
        catch (err) {
            this.output.appendLine(`[renium] editor live sync initial target scan failed: ${err instanceof Error ? err.message : String(err)}`);
            return { paths: [], targetSettingsIds: [] };
        }
        const rawPaths = Array.isArray(parsed.paths)
            ? parsed.paths
            : [];
        const rawIds = Array.isArray(parsed.targetSettingsIds)
            ? parsed.targetSettingsIds
            : [];
        const validSettingsPaths = new Set(settingsPaths.map((settingsPath) => path.resolve(settingsPath).toLowerCase()));
        const paths = rawPaths
            .map((value) => String(value))
            .filter((value) => validSettingsPaths.has(path.resolve(value).toLowerCase()));
        return {
            paths,
            targetSettingsIds: [...new Set(rawIds.map((value) => String(value)).filter((value) => value.startsWith("editor:")))],
        };
    }
    editorLiveSyncCachePath(projectRoot) {
        return path.join(projectRoot, ".renium", "editor-live-sync-cache.json");
    }
    emptyEditorLiveSyncCache(projectRoot) {
        return {
            version: 1,
            projectRoot: path.resolve(projectRoot),
            updatedAtUnixMs: Date.now(),
            files: {},
        };
    }
    loadEditorLiveSyncCache(projectRoot) {
        const cachePath = this.editorLiveSyncCachePath(projectRoot);
        try {
            const parsed = JSON.parse(fs.readFileSync(cachePath, "utf8"));
            if (parsed &&
                parsed.version === 1 &&
                parsed.files &&
                typeof parsed.files === "object" &&
                !Array.isArray(parsed.files)) {
                return {
                    existed: true,
                    cache: {
                        version: 1,
                        projectRoot: typeof parsed.projectRoot === "string" ? parsed.projectRoot : path.resolve(projectRoot),
                        updatedAtUnixMs: typeof parsed.updatedAtUnixMs === "number" ? parsed.updatedAtUnixMs : 0,
                        files: Object.fromEntries(Object.entries(parsed.files).filter((entry) => typeof entry[1] === "string")),
                    },
                };
            }
        }
        catch {

        }
        return { existed: false, cache: this.emptyEditorLiveSyncCache(projectRoot) };
    }
    saveEditorLiveSyncCache(projectRoot, cache) {
        const cachePath = this.editorLiveSyncCachePath(projectRoot);
        fs.mkdirSync(path.dirname(cachePath), { recursive: true });
        const nextCache = {
            version: 1,
            projectRoot: path.resolve(projectRoot),
            updatedAtUnixMs: Date.now(),
            files: cache.files,
        };
        fs.writeFileSync(cachePath, `${JSON.stringify(nextCache, null, 2)}${os.EOL}`, "utf8");
    }
    editorLiveSyncCacheKey(filePath, projectRoot) {
        const absolutePath = path.resolve(projectRoot, filePath);
        const relative = path.relative(projectRoot, absolutePath);
        const normalized = relative.split(path.sep).join("/");
        return process.platform === "win32" ? normalized.toLowerCase() : normalized;
    }
    editorLiveSyncFileHash(filePath) {
        try {
            const stat = fs.statSync(filePath);
            if (!stat.isFile()) {
                return undefined;
            }
            const hash = crypto.createHash("sha256");
            hash.update(fs.readFileSync(filePath));
            return `sha256:${stat.size}:${hash.digest("hex")}`;
        }
        catch {
            return undefined;
        }
    }
    primeEditorLiveSyncCache(paths, cfg) {
        const cache = this.emptyEditorLiveSyncCache(cfg.projectRoot);
        for (const filePath of paths) {
            const hash = this.editorLiveSyncFileHash(filePath);
            if (hash) {
                cache.files[this.editorLiveSyncCacheKey(filePath, cfg.projectRoot)] = hash;
            }
        }
        this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
    }
    filterEditorLiveSyncChangedPaths(paths, cfg) {
        const { cache, existed } = this.loadEditorLiveSyncCache(cfg.projectRoot);
        const seen = new Set();
        const changed = [];
        const currentHashes = {};
        for (const filePath of paths) {
            const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
            if (!seen.add(key)) {
                continue;
            }
            const hash = this.editorLiveSyncFileHash(filePath);
            if (hash) {
                currentHashes[key] = hash;
            }
            if (!existed) {
                continue;
            }
            if (hash === undefined) {
                if (cache.files[key] !== undefined) {
                    changed.push(filePath);
                }
                continue;
            }
            if (cache.files[key] !== hash) {
                changed.push(filePath);
            }
        }
        if (!existed) {
            cache.files = currentHashes;
            this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
            this.output.appendLine(`[renium] editor live sync cache primed: ${Object.keys(currentHashes).length} file(s)`);
            return [];
        }
        return changed;
    }
    updateEditorLiveSyncCacheAfterPush(paths, cfg) {
        const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
        for (const filePath of paths) {
            const key = this.editorLiveSyncCacheKey(filePath, cfg.projectRoot);
            const hash = this.editorLiveSyncFileHash(filePath);
            if (hash) {
                cache.files[key] = hash;
            }
            else {
                delete cache.files[key];
            }
        }
        this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
    }
    async suppressStudioLiveSyncAfterEditorPush(paths, cfg) {
        if (!cfg.studioLiveSyncEnabled || !cfg.editorLiveSyncEnabled || !this.liveSyncWatcher) {
            return;
        }
        const services = [...new Set(paths
                .map((filePath) => this.detectServiceForPath(filePath, cfg.projectRoot, cfg.services))
                .filter((service) => typeof service === "string" && service.length > 0))];
        if (services.length === 0) {
            return;
        }
        try {
            await this.getStudioChangeState(cfg, services, {
                reset: true,
                start: true,
                suppressSeconds: Math.max(1, Math.min(4, cfg.studioLiveSyncPollMs / 1000 + 1.5)),
            });
        }
        catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            this.output.appendLine("[renium] Studio -> editor live sync suppress after editor push failed: " + message);
        }
    }
    replaceEditorLiveSyncCacheForServices(services, cfg) {
        const { cache } = this.loadEditorLiveSyncCache(cfg.projectRoot);
        const srcRoot = path.join(cfg.projectRoot, "src");
        const selectedServices = this.normalizeServices(services, cfg.services);
        const serviceDirs = selectedServices.map((service) => path.join(srcRoot, service));
        const currentHashes = {};
        for (const serviceDir of serviceDirs) {
            if (!fs.existsSync(serviceDir)) {
                continue;
            }
            for (const filePath of this.collectInitialEditorLiveSyncPaths(serviceDir)) {
                const hash = this.editorLiveSyncFileHash(filePath);
                if (hash) {
                    currentHashes[this.editorLiveSyncCacheKey(filePath, cfg.projectRoot)] = hash;
                }
            }
        }
        for (const cachedKey of Object.keys(cache.files)) {
            const absolutePath = path.join(cfg.projectRoot, cachedKey);
            if (serviceDirs.some((serviceDir) => this.isPathInside(absolutePath, serviceDir))) {
                delete cache.files[cachedKey];
            }
        }
        for (const [key, hash] of Object.entries(currentHashes)) {
            cache.files[key] = hash;
        }
        this.saveEditorLiveSyncCache(cfg.projectRoot, cache);
    }
    async stopLiveSync() {
        this.liveSyncStopRequested = true;
        if (!this.liveSyncWatcher) {
            this.disposeLiveSyncRuntime();
            await this.setEditorLiveSyncEnabled(false);
            if (this.liveSyncOwnsServe) {
                this.bridgeServeRequested = false;
                this.liveSyncOwnsServe = false;
                this.stopBridgeDaemon();
            }
            else if (!this.bridgeServeRequested) {
                this.stopBridgeDaemon();
            }
            vscode.window.showInformationMessage("Renium: live sync is not running.");
            return;
        }
        this.disposeLiveSyncRuntime();
        await this.setEditorLiveSyncEnabled(false);
        if (this.liveSyncOwnsServe) {
            this.bridgeServeRequested = false;
            this.liveSyncOwnsServe = false;
            this.stopBridgeDaemon();
        }
        else if (!this.bridgeServeRequested) {
            this.stopBridgeDaemon();
        }
        this.updateStatusBar();
        vscode.window.showInformationMessage("Renium: editor -> Studio live sync stopped.");
    }
    async pushEditorPathsNow(paths, options = {}) {
        const changedPaths = (Array.isArray(paths) ? paths : [paths])
            .map((value) => String(value))
            .filter((value) => value.length > 0);
        if (changedPaths.length === 0) {
            return;
        }
        const cfg = this.getConfig();
        if (!options.force && !cfg.editorLiveSyncEnabled) {
            this.disposeLiveSyncRuntime();
            this.updateStatusBar();
            return;
        }
        await this.enqueue(options.taskName ?? "Editor -> Studio sync", async () => {
            const runCfg = this.getConfig();
            if (!options.force && !runCfg.editorLiveSyncEnabled) {
                this.output.appendLine("[renium] editor direct sync cancelled: editor -> Studio live sync is off");
                return;
            }
            const pathsToPush = options.skipChangeFilter === true
                ? changedPaths
                : this.filterEditorLiveSyncChangedPaths(changedPaths, runCfg);
            if (pathsToPush.length === 0) {
                this.output.appendLine(`[renium] editor direct sync skipped: ${changedPaths.length} unchanged path(s)`);
                return;
            }
            this.output.appendLine(`[renium] editor direct sync flushing ${pathsToPush.length}/${changedPaths.length} changed path(s)`);
            await this.runEditorPush(pathsToPush, runCfg, options);
        });
    }
    async pushEditorPropertyNow(request) {
        const cfg = this.getConfig();
        if (!request.force && !cfg.editorLiveSyncEnabled) {
            return;
        }
        const service = String(request.service ?? "").trim();
        const property = String(request.property ?? "").trim();
        const pathSegments = Array.isArray(request.pathSegments)
            ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
            : [];
        if (!service || !property || pathSegments.length === 0) {
            throw new Error("Editor property push requires service, property, and path segments.");
        }
        const command = cfg.exportCliPath;
        this.ensureFileExists(command);
        const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
        const args = [
            "prop",
            "-w",
            String(bridgeWaitSeconds),
            "-P",
            cfg.bridgePorts,
            "-s",
            service,
            "-c",
            String(request.className ?? ""),
            "-p",
            JSON.stringify(pathSegments),
            "-o",
            JSON.stringify(Array.isArray(request.pathOrdinals) ? request.pathOrdinals : []),
            "-S",
            request.scope ?? "property",
            "-n",
            property,
            "-v",
            JSON.stringify(request.value ?? null),
        ];
        const allowProtectedMeshIdApply = request.allowProtectedMeshIdApply === true
            || (property === "MeshId" && request.className === "MeshPart");
        if (allowProtectedMeshIdApply) {
            args.push("-m");
        }
        const settingsId = String(request.settingsId ?? "").trim();
        if (settingsId.length > 0) {
            args.push("-i", settingsId);
        }
        const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
        let result;
        if (usePersistentBridge) {
            result = await this.runDaemonCommand(command, args.slice(1), cfg, "editor-property", "prop", { quietWait: true });
        }
        else {
            result = await this.runCommand(command, args, cfg.projectRoot, "editor-property", cfg.progressHeartbeatSeconds);
        }
        if (result.code !== 0) {
            throw new Error(`Editor property push exited with code ${result.code}`);
        }
        const summary = this.parseEditorPushSummary(result.output);
        if (!summary) {
            throw new Error("Editor property push did not return a Studio apply result.");
        }
        const errors = this.summaryNumber(summary, "errors");
        if (summary.ok === false || errors > 0) {
            throw new Error("Studio rejected or failed editor property apply.");
        }
        const settingsFile = String(request.settingsFile ?? "").trim();
        if (settingsFile.length > 0 && fs.existsSync(settingsFile)) {
            this.updateEditorLiveSyncCacheAfterPush([settingsFile], cfg);
        }
    }
    async pushEditorDeleteNow(request) {
        const cfg = this.getConfig();
        if (!request.force && !cfg.editorLiveSyncEnabled) {
            return;
        }
        const service = String(request.service ?? "").trim();
        const pathSegments = Array.isArray(request.pathSegments)
            ? request.pathSegments.map((segment) => String(segment)).filter((segment) => segment.length > 0)
            : [];
        if (!service || pathSegments.length <= 1) {
            throw new Error("Editor delete push requires service and a non-root path.");
        }
        const command = cfg.exportCliPath;
        this.ensureFileExists(command);
        const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
        const args = [
            "del",
            "-w",
            String(bridgeWaitSeconds),
            "-P",
            cfg.bridgePorts,
            "-s",
            service,
            "-c",
            String(request.className ?? ""),
            "-p",
            JSON.stringify(pathSegments),
            "-o",
            JSON.stringify(Array.isArray(request.pathOrdinals) ? request.pathOrdinals : []),
        ];
        const settingsId = String(request.settingsId ?? "").trim();
        if (settingsId.length > 0) {
            args.push("-i", settingsId);
        }
        const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
        const result = usePersistentBridge
            ? await this.runDaemonCommand(command, args.slice(1), cfg, "editor-delete", "del", { quietWait: true })
            : await this.runCommand(command, args, cfg.projectRoot, "editor-delete", cfg.progressHeartbeatSeconds);
        if (result.code !== 0) {
            throw new Error(`Editor delete push exited with code ${result.code}`);
        }
        const summary = this.parseEditorPushSummary(result.output);
        if (!summary) {
            throw new Error("Editor delete push did not return a Studio apply result.");
        }
        const errors = this.summaryNumber(summary, "errors");
        if (summary.ok === false || errors > 0) {
            throw new Error("Studio rejected or failed editor delete apply.");
        }
        const settingsFile = String(request.settingsFile ?? "").trim();
        if (settingsFile.length > 0 && fs.existsSync(settingsFile)) {
            this.updateEditorLiveSyncCacheAfterPush([settingsFile], cfg);
        }
    }
    async onDocumentSaved(doc) {
        if (doc.isUntitled || doc.uri.scheme !== "file") {
            return;
        }
        const cfg = this.getConfig();
        if (!cfg.editorLiveSyncEnabled) {
            this.disposeLiveSyncRuntime();
            this.updateStatusBar();
        }
        if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && this.isPathInside(doc.uri.fsPath, path.join(cfg.projectRoot, "src"))) {
            const fileKey = this.normalizePathForCompare(doc.uri.fsPath);
            this.recentDirectSaveAtByPath.set(fileKey, Date.now());
            this.pendingEditorPaths.add(doc.uri.fsPath);
            if (this.liveSyncTimer) {
                clearTimeout(this.liveSyncTimer);
                this.liveSyncTimer = undefined;
            }
            void this.flushEditorChanges().catch((err) => {
                this.reportEditorLiveSyncError(err);
            });
            return;
        }
        if (!cfg.autoSyncOnSave) {
            return;
        }
        const service = this.detectServiceForPath(doc.uri.fsPath, cfg.projectRoot, cfg.services);
        if (service) {
            this.pendingAutoServices.add(service);
        }
        else {
            cfg.services.forEach((s) => this.pendingAutoServices.add(s));
        }
        if (this.autoSyncTimer) {
            clearTimeout(this.autoSyncTimer);
        }
        this.autoSyncTimer = setTimeout(() => {
            const services = Array.from(this.pendingAutoServices);
            this.pendingAutoServices.clear();
            void this.enqueue("Auto sync on save", async () => {
                await this.runExport({
                    services,
                    runImport: cfg.runImport,
                    notifyOnSuccess: false,
                    reason: "",
                });
            }).catch(() => undefined);
        }, Math.max(100, cfg.autoSyncDebounceMs));
    }
    queueEditorChange(filePath, immediate = false) {
        const cfg = this.getConfig();
        if (!cfg.editorLiveSyncEnabled) {
            this.disposeLiveSyncRuntime();
            this.updateStatusBar();
            return;
        }
        const srcRoot = path.join(cfg.projectRoot, "src");
        if (!this.isPathInside(filePath, srcRoot)) {
            return;
        }
        if (this.studioToEditorImportInProgress || Date.now() < this.studioToEditorImportSuppressUntilMs) {
            return;
        }
        if (!immediate) {
            const fileKey = this.normalizePathForCompare(filePath);
            const lastDirectSaveAt = this.recentDirectSaveAtByPath.get(fileKey) ?? 0;
            if (Date.now() - lastDirectSaveAt < 1000) {
                return;
            }
            if (this.recentDirectSaveAtByPath.size > 256) {
                this.recentDirectSaveAtByPath.clear();
            }
        }
        this.pendingEditorPaths.add(filePath);
        if (this.liveSyncTimer) {
            clearTimeout(this.liveSyncTimer);
        }
        const liveSyncDelayMs = immediate ? 0 : Math.max(50, Math.min(100, cfg.autoSyncDebounceMs));
        this.liveSyncTimer = setTimeout(() => {
            this.liveSyncTimer = undefined;
            void this.flushEditorChanges().catch((err) => {
                this.reportEditorLiveSyncError(err);
            });
        }, liveSyncDelayMs);
    }
    reportEditorLiveSyncError(err) {
        const message = err instanceof Error ? err.message : String(err);
        this.output.appendLine(`[renium] editor live sync failed: ${message}`);
        this.output.show(true);
        vscode.window.showErrorMessage(`Renium: editor live sync failed. ${message}`);
    }
    async flushEditorChanges() {
        const cfg = this.getConfig();
        if (!cfg.editorLiveSyncEnabled) {
            this.pendingEditorPaths.clear();
            return;
        }
        const queuedPaths = Array.from(this.pendingEditorPaths);
        this.pendingEditorPaths.clear();
        if (queuedPaths.length === 0) {
            return;
        }
        const changedPaths = this.filterEditorLiveSyncChangedPaths(queuedPaths, cfg);
        if (changedPaths.length === 0) {
            this.output.appendLine(`[renium] editor live sync skipped: ${queuedPaths.length} unchanged path(s)`);
            return;
        }
        await this.enqueue("Editor -> Studio sync", async () => {
            this.output.appendLine(`[renium] editor live sync flushing ${changedPaths.length}/${queuedPaths.length} changed path(s)`);
            await this.runEditorPush(changedPaths, cfg);
        });
    }
    async runEditorPush(changedPaths, cfg, options = {}) {
        const command = cfg.exportCliPath;
        this.ensureFileExists(command);
        const bridgeWaitSeconds = this.editorBridgeWaitSeconds(cfg);
        const args = [
            "push",
            "-r",
            cfg.projectRoot,
            "-d",
            "src",
            "-w",
            String(bridgeWaitSeconds),
            "-P",
            cfg.bridgePorts,
            "-v",
        ];
        let changedPathsFile;
        const changedPathArgs = changedPaths.map((changedPath) => this.editorChangedPathArg(changedPath, cfg.projectRoot));
        if (changedPathArgs.length > 32) {
            const listDir = path.join(cfg.projectRoot, ".renium", "editor-push-paths");
            fs.mkdirSync(listDir, { recursive: true });
            changedPathsFile = path.join(listDir, `paths-${Date.now()}-${Math.random().toString(16).slice(2)}.txt`);
            fs.writeFileSync(changedPathsFile, `${changedPathArgs.join(os.EOL)}${os.EOL}`, "utf8");
            args.push("-f", changedPathsFile);
        }
        else {
            for (const changedPath of changedPathArgs) {
                args.push("-p", changedPath);
            }
        }
        const targetSettingsId = typeof options.targetSettingsId === "string" ? options.targetSettingsId.trim() : "";
        const targetSettingsIds = [
            ...(targetSettingsId.length > 0 ? [targetSettingsId] : []),
            ...(Array.isArray(options.targetSettingsIds) ? options.targetSettingsIds : []),
        ]
            .map((value) => String(value).trim())
            .filter((value) => value.length > 0);
        const uniqueTargetSettingsIds = [...new Set(targetSettingsIds)];
        if (uniqueTargetSettingsIds.length > 128) {
            const listDir = path.join(cfg.projectRoot, ".renium", "editor-push-paths");
            fs.mkdirSync(listDir, { recursive: true });
            const targetIdsFile = path.join(listDir, `target-settings-${Date.now()}-${Math.random().toString(16).slice(2)}.txt`);
            fs.writeFileSync(targetIdsFile, `${uniqueTargetSettingsIds.join(os.EOL)}${os.EOL}`, "utf8");
            args.push("-I", targetIdsFile);
        }
        else {
            for (const targetId of uniqueTargetSettingsIds) {
                args.push("-i", targetId);
            }
        }
        const targetProperties = [
            ...(typeof options.targetProperty === "string" ? [options.targetProperty] : []),
            ...(Array.isArray(options.targetProperties) ? options.targetProperties : []),
        ]
            .map((value) => String(value).trim())
            .filter((value) => value.length > 0);
        for (const targetProperty of [...new Set(targetProperties)]) {
            args.push("-t", targetProperty);
        }
        if (options.upsertInstancesOnly === true) {
            args.push("-u");
        }
        const usePersistentBridge = this.shouldUsePersistentBridgeForEditorPush(cfg);
        if (usePersistentBridge) {
            this.output.appendLine(`[renium] editor push daemon request: ${this.renderArgs(args.slice(1))}`);
        }
        else {
            this.output.appendLine(`[renium] editor push command: ${command} ${this.renderArgs(args)}`);
        }
        try {
            const maxAttempts = 2;
            let result;
            for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
                if (usePersistentBridge) {
                    result = await this.runDaemonCommand(command, args.slice(1), cfg, attempt === 1 ? "editor-push" : `editor-push-retry-${attempt}`, "push");
                }
                else {
                    result = await this.runCommand(command, args, cfg.projectRoot, attempt === 1 ? "editor-push" : `editor-push-retry-${attempt}`, cfg.progressHeartbeatSeconds);
                }
                if (result.code === 0) {
                    break;
                }
                if (attempt >= maxAttempts || !isTransientBridgeFailure(result.output)) {
                    break;
                }
                await sleep(250);
            }
            if (!result || result.code !== 0) {
                throw new Error(`Editor push exited with code ${result?.code ?? "unknown"}`);
            }
            const summary = this.parseEditorPushSummary(result.output);
            if (!summary) {
                throw new Error("Editor push did not return a Studio apply result.");
            }
            const sourceVerified = this.summaryNumber(summary, "sourceVerified");
            const sourceVerifyFailed = this.summaryNumber(summary, "sourceVerifyFailed");
            const errors = this.summaryNumber(summary, "errors");
            const sourceQueued = this.summaryNumber(summary, "sourceQueued");
            const sourceUpdated = this.summaryNumber(summary, "sourceUpdated");
            const noops = this.summaryNumber(summary, "noops");
            this.output.appendLine(`[renium] editor push result: sourceQueued=${sourceQueued} sourceUpdated=${sourceUpdated} sourceVerified=${sourceVerified} sourceVerifyFailed=${sourceVerifyFailed} noops=${noops} errors=${errors}`);
            if (errors > 0) {
                const detail = Array.isArray(summary.sourceVerifyErrors) ? ` ${summary.sourceVerifyErrors.join("; ")}` : "";
                throw new Error(`Studio rejected or failed editor Source verification.${detail}`);
            }
            if (summary.ok === false || sourceVerifyFailed > 0) {
                const detail = Array.isArray(summary.sourceVerifyErrors) ? ` ${summary.sourceVerifyErrors.join("; ")}` : "";
                this.output.appendLine(`[renium] editor push verification warning:${detail || " Studio reported a source verification mismatch after apply."}`);
            }
            if (summary.ok !== false && errors === 0 && sourceVerifyFailed === 0) {
                this.updateEditorLiveSyncCacheAfterPush(changedPaths, cfg);
                await this.suppressStudioLiveSyncAfterEditorPush(changedPaths, cfg);
            }
            const existingSourceSaves = changedPaths.filter((changedPath) => this.isLuaSourcePath(changedPath) && fs.existsSync(changedPath)).length;
            if (existingSourceSaves > 0 && sourceVerified < existingSourceSaves) {
                this.output.appendLine(`[renium] editor push verification warning: verified ${sourceVerified}/${existingSourceSaves} saved Lua source file(s).`);
            }
        }
        finally {
            if (changedPathsFile) {
                try {
                    fs.unlinkSync(changedPathsFile);
                }
                catch {

                }
            }
        }
    }
    parseEditorPushSummary(output) {
        const prefix = "__ROBLOX_SYNC_EDITOR_PUSH_RESULT__ ";
        let found;
        for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
            const line = rawLine.trim();
            const index = line.indexOf(prefix);
            if (index < 0) {
                continue;
            }
            try {
                const parsed = JSON.parse(line.slice(index + prefix.length));
                if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
                    found = parsed;
                }
            }
            catch {

            }
        }
        return found;
    }
    parseStudioChangeState(output) {
        const prefix = "__ROBLOX_SYNC_STUDIO_CHANGE_STATE__ ";
        let found;
        for (const rawLine of output.replace(/\r\n/g, "\n").split("\n")) {
            const line = rawLine.trim();
            const index = line.indexOf(prefix);
            if (index < 0) {
                continue;
            }
            try {
                const parsed = JSON.parse(line.slice(index + prefix.length));
                if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
                    const record = parsed;
                    found = {
                        ok: typeof record.ok === "boolean" ? record.ok : undefined,
                        tracking: typeof record.tracking === "boolean" ? record.tracking : undefined,
                        role: typeof record.role === "string" ? record.role : undefined,
                        seq: typeof record.seq === "number" ? record.seq : undefined,
                        dirtyServices: Array.isArray(record.dirtyServices)
                            ? record.dirtyServices.map((value) => String(value))
                            : undefined,
                        trackedServices: typeof record.trackedServices === "number" ? record.trackedServices : undefined,
                        itemChangedAvailable: typeof record.itemChangedAvailable === "boolean" ? record.itemChangedAvailable : undefined,
                    };
                }
            }
            catch {

            }
        }
        return found;
    }
    summaryNumber(summary, key) {
        const value = summary[key];
        return typeof value === "number" && Number.isFinite(value) ? value : 0;
    }
    isLuaSourcePath(filePath) {
        return /\.(lua|luau)$/i.test(filePath);
    }
    onConfigurationChanged(event) {
        const cfg = this.getConfig();
        const bridgeConfigChanged = !event || [
            "renium.exportCliPath",
            "renium.projectRoot",
            "renium.transport",
            "renium.bridgeWaitSeconds",
            "renium.bridgePorts",
        ].some((key) => event.affectsConfiguration(key));
        const persistentBridgeChanged = event?.affectsConfiguration("renium.usePersistentBridge") === true;
        if (bridgeConfigChanged || (persistentBridgeChanged && !this.bridgeServeRequested)) {
            this.stopBridgeDaemon();
            if (this.bridgeServeRequested) {
                void this.serve({ silent: true, bestEffort: true });
            }
            else if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && this.shouldUsePersistentBridge(cfg)) {
                void this.prewarmPersistentBridgeDaemon("configuration");
            }
        }
        if (!cfg.editorLiveSyncEnabled && this.liveSyncWatcher) {
            this.disposeLiveSyncRuntime();
            if (!this.bridgeServeRequested) {
                this.stopBridgeDaemon();
            }
        }
        if (cfg.editorLiveSyncEnabled && this.liveSyncWatcher && !this.liveSyncStartupInProgress) {
            if (cfg.studioLiveSyncEnabled) {
                void this.startStudioLiveSyncRuntime(cfg, { bestEffort: true });
            }
            else {
                this.stopStudioLiveSyncRuntime();
            }
        }
        this.updateStatusBar();
    }
    async prewarmPersistentBridgeDaemon(reason = "activation") {
        const cfg = this.getConfig();
        if (!this.shouldUsePersistentBridge(cfg)) {
            return;
        }
        if (!this.bridgeServeRequested && !(cfg.editorLiveSyncEnabled && this.liveSyncWatcher)) {
            return;
        }
        if (!fs.existsSync(cfg.exportCliPath)) {
            this.output.appendLine(`[renium] bridge daemon prewarm skipped (${reason}): export CLI does not exist yet: ${cfg.exportCliPath}`);
            return;
        }
        try {
            await this.ensureBridgeDaemon(cfg.exportCliPath, cfg, { serve: this.bridgeServeRequested });
            this.output.appendLine(`[renium] bridge daemon prewarm ready (${reason})`);
        }
        catch (err) {
            const message = err instanceof Error ? err.message : String(err);
            this.output.appendLine(`[renium] bridge daemon prewarm skipped (${reason}): ${message}`);
        }
    }
    async toggleAutoSyncOnSave() {
        const cfg = vscode.workspace.getConfiguration("renium");
        const enabled = cfg.get("autoSyncOnSave", false);
        await cfg.update("autoSyncOnSave", !enabled, vscode.ConfigurationTarget.Workspace);
        this.updateStatusBar();
        vscode.window.showInformationMessage(`Renium: auto sync on save ${!enabled ? "enabled" : "disabled"}.`);
    }
    async enqueue(taskName, task) {
        const run = async () => {
            try {
                this.setActiveTask(taskName);
                this.output.appendLine(`[renium] task start: ${taskName}`);
                await task();
                this.output.appendLine(`[renium] task done: ${taskName}`);
            }
            catch (err) {
                const message = err instanceof Error ? err.message : String(err);
                this.output.appendLine(`[renium] task failed: ${taskName}: ${message}`);
                this.output.show(true);
                vscode.window.showErrorMessage(`Renium: ${taskName} failed. ${message}`);
                throw err;
            }
            finally {
                this.setActiveTask(undefined);
            }
        };
        this.queue = this.queue.then(run, run);
        await this.queue;
    }
    async runExport(options) {
        const cfg = {
            ...this.getConfig(),
            ...(options.configOverrides ?? {}),
        };
        const selectedServices = this.normalizeServices(options.services, cfg.services);
        const useRustImportInExporter = options.runImport;
        const { command, args } = this.resolveExportCommand(cfg, selectedServices, options.runImport, useRustImportInExporter, options.quietTimings !== false);
        const usePersistentBridge = this.shouldUsePersistentBridge(cfg);
        const quietLog = options.quietLog === true;
        if (!quietLog) {
            this.output.show(false);
            this.logResolvedConfig(cfg);
            if (usePersistentBridge) {
                this.output.appendLine(`[renium] export daemon command: ${command} bd -w ${Math.max(1, cfg.bridgeWaitSeconds)} -P ${cfg.bridgePorts}`);
                this.output.appendLine(`[renium] export daemon request: x ${this.renderArgs(args.slice(1))}`);
            }
            else {
                this.output.appendLine(`[renium] export command: ${command} ${this.renderArgs(args)}`);
            }
        }
        const maxAttempts = 3;
        let result;
        for (let attempt = 1; attempt <= maxAttempts; attempt += 1) {
            if (usePersistentBridge) {
                result = await this.runDaemonExport(command, args.slice(1), cfg, attempt === 1 ? "export" : `export-retry-${attempt}`, { quietWait: quietLog });
            }
            else {
                result = await this.runCommand(command, args, cfg.projectRoot, attempt === 1 ? "export" : `export-retry-${attempt}`, cfg.progressHeartbeatSeconds);
            }
            if (result.code === 0) {
                break;
            }
            if (attempt >= maxAttempts || !isTransientBridgeFailure(result.output)) {
                break;
            }
            const retryDelayMs = attempt === 1 ? 250 : 500;
            if (!quietLog) {
                this.output.appendLine(`[renium] export: transient bridge failure; retrying attempt ${attempt + 1}/${maxAttempts} after ${retryDelayMs}ms`);
            }
            await sleep(retryDelayMs);
        }
        if (!result || result.code !== 0) {
            throw new Error(`Export exited with code ${result?.code ?? "unknown"}`);
        }
        if (options.runImport && options.notifyOnSuccess) {
            try {
                await vscode.commands.executeCommand("renium.fileExplorer.refreshServices", selectedServices);
            }
            catch {

            }
        }
        if (options.notifyOnSuccess && options.reason) {
            vscode.window.showInformationMessage(`Renium: ${options.reason}.`);
        }
        return result;
    }
    daemonKey(command, cfg, serve) {
        let binaryMtimeMs = 0;
        try {
            binaryMtimeMs = Math.floor(fs.statSync(command).mtimeMs);
        }
        catch {
            binaryMtimeMs = 0;
        }
        return JSON.stringify({
            command,
            binaryMtimeMs,
            projectRoot: cfg.projectRoot,
            bridgePorts: cfg.bridgePorts,
            bridgeWaitSeconds: Math.max(1, cfg.bridgeWaitSeconds),
            serve,
        });
    }
    async runDaemonExport(command, args, cfg, label, options = {}) {
        return await this.runDaemonCommand(command, args, cfg, label, "x", options);
    }
    async runDaemonCommand(command, args, cfg, label, daemonCommand, options = {}) {
        await this.ensureBridgeDaemon(command, cfg, { serve: this.bridgeServeRequested });
        return await new Promise((resolve, reject) => {
            const proc = this.daemonProcess;
            if (!proc || proc.killed || !proc.stdin?.writable) {
                reject(new Error("Persistent bridge daemon is not running."));
                return;
            }
            const launchedAt = Date.now();
            const id = this.daemonRequestId++;
            const pending = {
                id,
                label,
                launchedAt,
                lastOutputAt: launchedAt,
                sawOutput: false,
                output: "",
                resolve,
                reject,
                heartbeatTimer: undefined,
                quiet: options.quietWait === true,
            };
            if (!options.quietWait) {
                const heartbeatMs = Math.max(2, Math.round(cfg.progressHeartbeatSeconds)) * 1000;
                pending.heartbeatTimer = setInterval(() => {
                    const now = Date.now();
                    const elapsedSec = ((now - launchedAt) / 1000).toFixed(1);
                    const idleSec = ((now - pending.lastOutputAt) / 1000).toFixed(1);
                    if (!pending.sawOutput) {
                        this.output.appendLine(`[renium] ${label}: waiting for daemon output (${elapsedSec}s elapsed)`);
                    }
                    else {
                        this.output.appendLine(`[renium] ${label}: daemon still running (${elapsedSec}s elapsed, idle ${idleSec}s)`);
                    }
                }, heartbeatMs);
            }
            this.daemonPending.set(id, pending);
            const request = JSON.stringify({
                id,
                command: daemonCommand,
                args,
            }) + "\n";
            proc.stdin.write(request, "utf8", (err) => {
                if (err) {
                    this.finishDaemonRequest(id, {
                        code: 1,
                        output: pending.output + `\n[renium] daemon request write failed: ${err.message}`,
                    });
                }
            });
        });
    }
    shouldUsePersistentBridge(cfg) {
        return cfg.transport === "ws" && (cfg.usePersistentBridge || this.bridgeServeRequested);
    }
    shouldUsePersistentBridgeForEditorPush(cfg) {
        return cfg.transport === "ws" && (this.bridgeServeRequested ||
            (cfg.editorLiveSyncEnabled && this.liveSyncWatcher !== undefined) ||
            (cfg.usePersistentBridge && this.isBridgeDaemonRunning()));
    }
    editorBridgeWaitSeconds(cfg) {
        return Math.max(1, Math.min(2, Number(cfg.bridgeWaitSeconds) || 2));
    }
    isBridgeDaemonRunning() {
        return !!this.daemonProcess && !this.daemonProcess.killed;
    }
    async ensureBridgeDaemon(command, cfg, options = {}) {
        this.ensureFileExists(command);
        const key = this.daemonKey(command, cfg, options.serve === true);
        if (this.daemonProcess && !this.daemonProcess.killed && this.daemonKeyValue === key) {
            await this.awaitBridgeDaemonReady(cfg);
            return;
        }
        this.stopBridgeDaemon();
        const args = [
            "bd",
            "-w",
            String(Math.max(1, cfg.bridgeWaitSeconds)),
            "-P",
            cfg.bridgePorts,
            options.serve ? "-s" : "",
        ].filter((value) => value.length > 0);
        const child = childProcess.spawn(command, args, {
            cwd: cfg.projectRoot,
            env: process.env,
            shell: false,
            stdio: "pipe",
            windowsHide: true,
        });
        this.daemonProcess = child;
        this.daemonKeyValue = key;
        this.daemonOutputBuffer = "";
        this.daemonReady = false;
        this.daemonReadyPromise = new Promise((resolve, reject) => {
            this.daemonReadyResolve = resolve;
            this.daemonReadyReject = reject;
        });
        this.output.appendLine(`[renium] bridge daemon: spawned pid=${child.pid ?? "unknown"}`);
        child.stdout.on("data", (data) => {
            this.handleDaemonOutput(command, data, false);
        });
        child.stderr.on("data", (data) => {
            this.handleDaemonOutput(`${command}:err`, data, true);
        });
        child.on("error", (err) => {
            this.output.appendLine(`[renium] bridge daemon error: ${err.message}`);
            this.daemonReadyReject?.(err);
            this.rejectDaemonPending(err);
        });
        child.on("exit", (code) => {
            const exitError = new Error(`Persistent bridge daemon exited with code ${code ?? 0}`);
            this.output.appendLine(`[renium] bridge daemon exited code=${code ?? 0}`);
            this.daemonReadyReject?.(exitError);
            this.rejectDaemonPending(exitError);
            if (this.daemonProcess === child) {
                this.daemonProcess = undefined;
                this.daemonKeyValue = undefined;
                this.daemonOutputBuffer = "";
                this.daemonReady = false;
                this.daemonReadyPromise = undefined;
                this.daemonReadyResolve = undefined;
                this.daemonReadyReject = undefined;
            }
        });
        await this.awaitBridgeDaemonReady(cfg);
    }
    async awaitBridgeDaemonReady(cfg) {
        if (this.daemonReady) {
            return;
        }
        const readyPromise = this.daemonReadyPromise;
        if (!readyPromise) {
            throw new Error("Persistent bridge daemon was not started.");
        }
        const timeoutMs = Math.max(1, cfg.bridgeWaitSeconds + 2) * 1000;
        let timeoutHandle;
        try {
            await Promise.race([
                readyPromise,
                new Promise((_resolve, reject) => {
                    timeoutHandle = setTimeout(() => {
                        reject(new Error(`Persistent bridge daemon did not become ready within ${Math.round(timeoutMs / 1000)}s.`));
                    }, timeoutMs);
                }),
            ]);
        }
        finally {
            if (timeoutHandle) {
                clearTimeout(timeoutHandle);
            }
        }
    }
    handleDaemonOutput(prefix, data, isStderr) {
        const text = data.toString();
        const hasQuietPending = Array.from(this.daemonPending.values()).some((pending) => pending.quiet);
        if (!hasQuietPending) {
            this.output.append(this.prefixOutput(prefix, data));
        }
        for (const pending of this.daemonPending.values()) {
            pending.output += text;
            if (pending.output.length > 8000000) {
                pending.output = pending.output.slice(-8000000);
            }
            pending.sawOutput = true;
            pending.lastOutputAt = Date.now();
        }
        if (isStderr) {
            return;
        }
        this.daemonOutputBuffer += text;
        let newlineIndex = this.daemonOutputBuffer.indexOf("\n");
        while (newlineIndex >= 0) {
            const line = this.daemonOutputBuffer.slice(0, newlineIndex).replace(/\r$/, "");
            this.daemonOutputBuffer = this.daemonOutputBuffer.slice(newlineIndex + 1);
            this.processDaemonLine(line);
            newlineIndex = this.daemonOutputBuffer.indexOf("\n");
        }
    }
    processDaemonLine(line) {
        const readyPrefix = "__ROBLOX_SYNC_DAEMON_READY__ ";
        if (line.startsWith(readyPrefix)) {
            this.daemonReady = true;
            this.daemonReadyResolve?.();
            this.daemonReadyResolve = undefined;
            this.daemonReadyReject = undefined;
            return;
        }
        const resultPrefix = "__ROBLOX_SYNC_DAEMON_RESULT__ ";
        if (!line.startsWith(resultPrefix)) {
            return;
        }
        let payload;
        try {
            payload = JSON.parse(line.slice(resultPrefix.length));
        }
        catch (err) {
            this.output.appendLine(`[renium] bridge daemon: invalid result sentinel: ${err instanceof Error ? err.message : String(err)}`);
            return;
        }
        const record = payload;
        const id = Number(record.id ?? 0);
        const code = Number(record.code ?? (record.ok ? 0 : 1));
        const pending = this.daemonPending.get(id);
        if (!pending) {
            return;
        }
        let output = pending.output;
        if (code !== 0 && record.error) {
            output += `\n[renium] daemon request error: ${String(record.error)}\n`;
        }
        this.finishDaemonRequest(id, { code, output });
    }
    finishDaemonRequest(id, result) {
        const pending = this.daemonPending.get(id);
        if (!pending) {
            return;
        }
        if (pending.heartbeatTimer) {
            clearInterval(pending.heartbeatTimer);
        }
        this.daemonPending.delete(id);
        const elapsedSec = ((Date.now() - pending.launchedAt) / 1000).toFixed(1);
        if (!pending.quiet) {
            this.output.appendLine(`[renium] ${pending.label}: daemon result code=${result.code} after ${elapsedSec}s`);
        }
        pending.resolve(result);
    }
    rejectDaemonPending(err) {
        for (const [id, pending] of this.daemonPending.entries()) {
            if (pending.heartbeatTimer) {
                clearInterval(pending.heartbeatTimer);
            }
            this.daemonPending.delete(id);
            pending.reject(err);
        }
    }
    sendDaemonShutdown() {
        const proc = this.daemonProcess;
        if (!proc || proc.killed || !proc.stdin?.writable) {
            return;
        }
        try {
            const id = this.daemonRequestId++;
            proc.stdin.write(JSON.stringify({ id, command: "shutdown", args: [] }) + "\n", "utf8");
        }
        catch {

        }
    }
    stopBridgeDaemon() {
        const proc = this.daemonProcess;
        if (!proc) {
            this.daemonReady = false;
            this.daemonReadyPromise = undefined;
            this.daemonReadyResolve = undefined;
            this.daemonReadyReject = undefined;
            return;
        }
        this.sendDaemonShutdown();
        if (!proc.killed) {
            proc.kill();
        }
        this.daemonReadyReject?.(new Error("Persistent bridge daemon was stopped."));
        this.daemonProcess = undefined;
        this.daemonKeyValue = undefined;
        this.daemonOutputBuffer = "";
        this.daemonReady = false;
        this.daemonReadyPromise = undefined;
        this.daemonReadyResolve = undefined;
        this.daemonReadyReject = undefined;
    }
    resolveExportCommand(cfg, selectedServices, requestedRunImport, useRustImportInExporter, quietTimings) {
        const runImportFlag = requestedRunImport ? "-i" : "--no-import";
        const extraImportArgs = [];
        if (useRustImportInExporter) {
            this.ensureFileExists(cfg.rustCliPath);
            extraImportArgs.push("--import-cli", cfg.rustCliPath);
        }
        this.ensureFileExists(cfg.exportCliPath);
        return {
            command: cfg.exportCliPath,
            args: [
                "x",
                "-r",
                cfg.projectRoot,
                "-d",
                cfg.snapshotDir,
                "-t",
                cfg.transport,
                "-s",
                selectedServices.join(","),
                "--sw",
                String(Math.max(0, cfg.sourceWorkers)),
                "--iw",
                String(Math.max(0, cfg.instanceWorkers)),
                "--mw",
                String(Math.max(0, cfg.importWorkers)),
                "--perf",
                cfg.performanceMode,
                ...(cfg.modifiedDefaultBypass ? ["--mdb"] : ["--no-mdb"]),
                "-c",
                String(Math.max(512, cfg.chunkSize)),
                "--ic",
                String(Math.max(0, cfg.snapshotInstanceChunkSize)),
                "-w",
                String(Math.max(1, cfg.bridgeWaitSeconds)),
                "-P",
                cfg.bridgePorts,
                "-S",
                cfg.server,
                "-C",
                cfg.configTomlPath,
                "-W",
                String(Math.max(1, cfg.wsWaitSeconds)),
                "-m",
                cfg.importMode,
                runImportFlag,
                quietTimings ? "-q" : "",
                cfg.adaptiveThrottle ? "--adaptive-throttle" : "--no-adaptive-throttle",
                cfg.noUpdateEditorIcons ? "--no-icons" : "",
                ...extraImportArgs,
            ].filter((x) => x.length > 0),
        };
    }
    async runRustImport(cfg, snapshotPath, services) {
        this.ensureFileExists(cfg.rustCliPath);
        const selectedServices = this.normalizeServices(services, cfg.services);
        const args = [
            "import-snapshots",
            "--snapshot-dir",
            snapshotPath,
            "--project-root",
            cfg.projectRoot,
            "--services",
            selectedServices.join(","),
            "--compact-meta-json",
        ];
        this.output.show(false);
        this.logResolvedConfig(cfg);
        this.output.appendLine(`[renium] rust import command: ${cfg.rustCliPath} ${this.renderArgs(args)}`);
        const result = await this.runCommand(cfg.rustCliPath, args, cfg.projectRoot, "rust-import", cfg.progressHeartbeatSeconds);
        if (result.code !== 0) {
            throw new Error(`Rust import exited with code ${result.code}`);
        }
    }
    resolveSnapshotPath(cfg) {
        return path.isAbsolute(cfg.snapshotDir) ? cfg.snapshotDir : path.join(cfg.projectRoot, cfg.snapshotDir);
    }
    async runCommand(command, args, cwd, label, progressHeartbeatSeconds) {
        return await new Promise((resolve, reject) => {
            const launchedAt = Date.now();
            let lastOutputAt = launchedAt;
            let sawOutput = false;
            let capturedOutput = "";
            const child = childProcess.spawn(command, args, {
                cwd,
                env: process.env,
                shell: false,
                stdio: "pipe",
                windowsHide: true,
            });
            this.output.appendLine(`[renium] ${label}: spawned pid=${child.pid ?? "unknown"} at ${new Date(launchedAt).toISOString()}`);
            const heartbeatMs = Math.max(2, Math.round(progressHeartbeatSeconds)) * 1000;
            const heartbeatTimer = setInterval(() => {
                const now = Date.now();
                const elapsedSec = ((now - launchedAt) / 1000).toFixed(1);
                const idleSec = ((now - lastOutputAt) / 1000).toFixed(1);
                if (!sawOutput) {
                    this.output.appendLine(`[renium] ${label}: waiting for first output (${elapsedSec}s elapsed)`);
                }
                else {
                    this.output.appendLine(`[renium] ${label}: still running (${elapsedSec}s elapsed, idle ${idleSec}s)`);
                }
            }, heartbeatMs);
            this.bindProcessOutput(child, command, () => {
                sawOutput = true;
                lastOutputAt = Date.now();
            }, (text) => {
                capturedOutput += text;
                if (capturedOutput.length > 8000000) {
                    capturedOutput = capturedOutput.slice(-8000000);
                }
            });
            child.on("error", (err) => {
                clearInterval(heartbeatTimer);
                reject(err);
            });
            child.on("exit", (code) => {
                clearInterval(heartbeatTimer);
                const elapsedSec = ((Date.now() - launchedAt) / 1000).toFixed(1);
                this.output.appendLine(`[renium] ${label}: exited code=${code ?? 0} after ${elapsedSec}s`);
                resolve({ code: code ?? 0, output: capturedOutput });
            });
        });
    }
    bindProcessOutput(child, prefix, onActivity, onChunk) {
        child.stdout?.on("data", (data) => {
            onActivity?.();
            onChunk?.(data.toString());
            this.output.append(this.prefixOutput(prefix, data));
        });
        child.stderr?.on("data", (data) => {
            onActivity?.();
            onChunk?.(data.toString());
            this.output.append(this.prefixOutput(`${prefix}:err`, data));
        });
    }
    prefixOutput(prefix, data) {
        const text = data.toString();
        const lines = text.replace(/\r\n/g, "\n").split("\n");
        if (lines.length === 1) {
            return `[${prefix}] ${lines[0]}`;
        }
        return lines
            .filter((line, index) => !(line.length === 0 && index === lines.length - 1))
            .map((line) => `[${prefix}] ${line}`)
            .join("\n") + "\n";
    }
    ensureFileExists(filePath) {
        if (!fs.existsSync(filePath)) {
            throw new Error(`Required file not found: ${filePath}`);
        }
    }
    normalizeServices(requested, fallback) {
        const requestedSet = new Set(requested.map((x) => x.trim()).filter((x) => x.length > 0));
        if (requestedSet.size === 0) {
            fallback.forEach((s) => requestedSet.add(s));
        }
        return Array.from(requestedSet);
    }
    getConfig() {
        const root = this.getWorkspaceRoot();
        const cfg = vscode.workspace.getConfiguration("renium");
        const projectRoot = this.resolveConfigPath(cfg.get("projectRoot", "${workspaceFolder}"), root);
        const configTomlPath = this.resolveConfigPath(cfg.get("configTomlPath", "${userHome}/.codex/config.toml"), root);
        const watchConfigPath = this.resolveConfigPath(cfg.get("watchConfigPath", "${workspaceFolder}/tools/editor_to_studio_sync.json"), root);
        const exportCliPath = this.resolveConfigPath(cfg.get("exportCliPath", "${workspaceFolder}/tools/renium/target/release/renium.exe"), root);
        const editorSyncCliPath = this.resolveConfigPath(cfg.get("editorSyncCliPath", "${workspaceFolder}/dist/editor_to_studio_sync.exe"), root);
        const servicesRaw = cfg.get("services", DEFAULT_SERVICES);
        const services = (Array.isArray(servicesRaw) ? servicesRaw : DEFAULT_SERVICES)
            .map((s) => String(s).trim())
            .filter((s) => s.length > 0);
        const transportRaw = cfg.get("transport", "ws");
        const transport = transportRaw === "mcp" ? "mcp" : "ws";
        const importModeRaw = cfg.get("importMode", "direct");
        const importMode = importModeRaw === "snapshot" ? "snapshot" : "direct";
        const performanceModeRaw = cfg.get("performanceMode", "throughput");
        const performanceMode = performanceModeRaw === "smooth"
            ? "smooth"
            : performanceModeRaw === "balanced"
                ? "balanced"
                : "throughput";
        const modifiedDefaultBypass = cfg.get("modifiedDefaultBypass", false) === true;
        const wsWaitSeconds = this.getWsWaitSeconds(cfg);
        const chunkSize = this.normalizeChunkSize(cfg);
        const rustCliPath = this.resolveConfigPath(cfg.get("rustCliPath", "${workspaceFolder}/tools/renium/target/release/renium.exe"), root);
        return {
            exportCliPath,
            editorSyncCliPath,
            rustCliPath,
            projectRoot,
            snapshotDir: cfg.get("snapshotDir", "snapshots"),
            transport,
            server: cfg.get("server", "Roblox_Studio"),
            configTomlPath,
            services: services.length > 0 ? services : [...DEFAULT_SERVICES],
            sourceWorkers: Number(cfg.get("sourceWorkers", 0) ?? 0),
            instanceWorkers: Number(cfg.get("instanceWorkers", 0) ?? 0),
            importWorkers: Number(cfg.get("importWorkers", 0) ?? 0),
            chunkSize,
            snapshotInstanceChunkSize: Number(cfg.get("snapshotInstanceChunkSize", 5000) ?? 5000),
            bridgeWaitSeconds: Number(cfg.get("bridgeWaitSeconds", 8) ?? 8),
            bridgePorts: this.normalizeBridgePorts(String(cfg.get("bridgePorts", DEFAULT_BRIDGE_PORTS.join(",")) ?? DEFAULT_BRIDGE_PORTS.join(","))),
            usePersistentBridge: cfg.get("usePersistentBridge", true) !== false,
            adaptiveThrottle: cfg.get("adaptiveThrottle", true),
            noUpdateEditorIcons: cfg.get("noUpdateEditorIcons", true),
            autoSyncOnSave: cfg.get("autoSyncOnSave", false),
            autoSyncDebounceMs: Number(cfg.get("autoSyncDebounceMs", 800) ?? 800),
            editorLiveSyncEnabled: this.editorLiveSyncRuntimeEnabled,
            editorLiveSyncOnStartup: cfg.get("editorLiveSyncOnStartup", false) === true,
            studioLiveSyncEnabled: cfg.get("studioLiveSyncEnabled", true) !== false,
            studioLiveSyncPollMs: Math.max(250, Number(cfg.get("studioLiveSyncPollMs", 500) ?? 500)),
            runImport: cfg.get("runImport", true),
            importMode,
            performanceMode,
            modifiedDefaultBypass,
            watchConfigPath,
            wsWaitSeconds,
            progressHeartbeatSeconds: Number(cfg.get("progressHeartbeatSeconds", 2) ?? 2),
            benchmarkRuns: Math.max(1, Math.floor(Number(cfg.get("benchmarkRuns", 5) ?? 5))),
        };
    }
    normalizeChunkSize(cfg) {
        const inspected = cfg.inspect("chunkSize");
        const configuredValue = inspected?.workspaceFolderValue ??
            inspected?.workspaceValue ??
            inspected?.globalValue ??
            inspected?.defaultValue;
        const rawValue = Number(configuredValue ?? DEFAULT_CHUNK_SIZE);
        if (!Number.isFinite(rawValue) || rawValue < 512) {
            return DEFAULT_CHUNK_SIZE;
        }
        if (rawValue <= 262144) {
            if (!this.warnedLegacyChunkSize) {
                this.warnedLegacyChunkSize = true;
                this.output.appendLine(`[renium] config: chunkSize 262144 is legacy; using ${DEFAULT_CHUNK_SIZE} for this run.`);
            }
            return DEFAULT_CHUNK_SIZE;
        }
        return Math.max(512, Math.floor(rawValue));
    }
    configOrigin(cfg, key) {
        const inspected = cfg.inspect(key);
        if (inspected?.workspaceFolderValue !== undefined) {
            return "workspace-folder";
        }
        if (inspected?.workspaceValue !== undefined) {
            return "workspace";
        }
        if (inspected?.globalValue !== undefined) {
            return "user";
        }
        if (inspected?.defaultValue !== undefined) {
            return "default";
        }
        return "unset";
    }
    configuredValue(cfg, key) {
        const inspected = cfg.inspect(key);
        return (inspected?.workspaceFolderValue ??
            inspected?.workspaceValue ??
            inspected?.globalValue ??
            inspected?.defaultValue);
    }
    logResolvedConfig(cfg) {
        const workspaceCfg = vscode.workspace.getConfiguration("renium");
        const extensionVersion = String(this.context.extension.packageJSON.version ?? "unknown");
        const extensionEntryPath = path.join(this.context.extensionPath, "out", "extension.js");
        const extensionBuildUnix = fs.existsSync(extensionEntryPath)
            ? Math.floor(fs.statSync(extensionEntryPath).mtimeMs / 1000)
            : 0;
        this.output.appendLine(`[renium] extension version=${extensionVersion}`);
        this.output.appendLine(`[renium] extension build_unix=${extensionBuildUnix}`);
        this.output.appendLine(`[renium] config: exportCliPath=${cfg.exportCliPath}`);
        this.output.appendLine(`[renium] config: rustCliPath=${cfg.rustCliPath}`);
        this.output.appendLine(`[renium] config: chunkSize=${cfg.chunkSize} (origin=${this.configOrigin(workspaceCfg, "chunkSize")}, raw=${String(this.configuredValue(workspaceCfg, "chunkSize"))})`);
        this.output.appendLine(`[renium] config: bridgePorts=${cfg.bridgePorts} (origin=${this.configOrigin(workspaceCfg, "bridgePorts")})`);
        this.output.appendLine(`[renium] config: usePersistentBridge=${cfg.usePersistentBridge} (origin=${this.configOrigin(workspaceCfg, "usePersistentBridge")})`);
        this.output.appendLine(`[renium] config: sourceWorkers=${cfg.sourceWorkers} (origin=${this.configOrigin(workspaceCfg, "sourceWorkers")})`);
        this.output.appendLine(`[renium] config: instanceWorkers=${cfg.instanceWorkers} (origin=${this.configOrigin(workspaceCfg, "instanceWorkers")})`);
        this.output.appendLine(`[renium] config: importWorkers=${cfg.importWorkers} (origin=${this.configOrigin(workspaceCfg, "importWorkers")})`);
        this.output.appendLine(`[renium] config: importMode=${cfg.importMode} (origin=${this.configOrigin(workspaceCfg, "importMode")})`);
        this.output.appendLine(`[renium] config: performanceMode=${cfg.performanceMode} (origin=${this.configOrigin(workspaceCfg, "performanceMode")})`);
        this.output.appendLine(`[renium] config: modifiedDefaultBypass=${cfg.modifiedDefaultBypass} (origin=${this.configOrigin(workspaceCfg, "modifiedDefaultBypass")})`);
        this.output.appendLine(`[renium] config: benchmarkRuns=${cfg.benchmarkRuns} (origin=${this.configOrigin(workspaceCfg, "benchmarkRuns")})`);
    }
    normalizeBridgePorts(raw) {
        const parsed = raw
            .split(",")
            .map((token) => Number.parseInt(token.trim(), 10))
            .filter((value) => Number.isInteger(value) && value > 0 && value <= 65535)
            .filter((value, index, all) => all.indexOf(value) === index);
        let normalized = parsed;
        const matchesPreviousDefault = normalized.length === PREVIOUS_DEFAULT_BRIDGE_PORTS.length &&
            normalized.every((value, index) => value === PREVIOUS_DEFAULT_BRIDGE_PORTS[index]);
        const matchesLegacyDefault = normalized.length === LEGACY_BRIDGE_PORTS.length &&
            normalized.every((value, index) => value === LEGACY_BRIDGE_PORTS[index]);
        if (matchesPreviousDefault || matchesLegacyDefault) {
            if (!this.warnedLegacyBridgePorts) {
                this.warnedLegacyBridgePorts = true;
                this.output.appendLine(`[renium] config: migrating legacy bridge default to ${DEFAULT_BRIDGE_PORTS.join(",")}.`);
            }
            normalized = [...DEFAULT_BRIDGE_PORTS];
        }
        if (normalized.length === 0) {
            normalized = [...DEFAULT_BRIDGE_PORTS];
        }
        if (normalized.length > DEFAULT_BRIDGE_PORTS.length) {
            if (!this.warnedBridgePortLimit) {
                this.warnedBridgePortLimit = true;
                this.output.appendLine(`[renium] config: only ${DEFAULT_BRIDGE_PORTS.length} bridge ports are supported; using ${normalized
                    .slice(0, DEFAULT_BRIDGE_PORTS.length)
                    .join(",")}.`);
            }
            normalized = normalized.slice(0, DEFAULT_BRIDGE_PORTS.length);
        }
        return normalized.join(",");
    }
    getWsWaitSeconds(cfg) {
        const configuredWsWaitSeconds = this.getConfiguredNumber(cfg, "wsWaitSeconds");
        if (configuredWsWaitSeconds !== undefined) {
            return configuredWsWaitSeconds;
        }
        const legacyStartupWaitSeconds = this.getConfiguredNumber(cfg, "startupWaitSeconds");
        if (legacyStartupWaitSeconds !== undefined) {
            if (!this.warnedLegacyStartupWaitSeconds) {
                this.warnedLegacyStartupWaitSeconds = true;
                this.output.appendLine("[renium] config: using legacy renium.startupWaitSeconds as renium.wsWaitSeconds; update your settings to renium.wsWaitSeconds.");
            }
            return legacyStartupWaitSeconds;
        }
        return Number(cfg.get("wsWaitSeconds", 20) ?? 20);
    }
    getConfiguredNumber(cfg, key) {
        const inspected = cfg.inspect(key);
        const configuredValue = inspected?.workspaceFolderValue ??
            inspected?.workspaceValue ??
            inspected?.globalValue;
        return configuredValue === undefined ? undefined : Number(configuredValue);
    }
    resolveConfigPath(raw, workspaceRoot) {
        const replaced = raw
            .replaceAll("${workspaceFolder}", workspaceRoot)
            .replaceAll("${userHome}", os.homedir());
        return path.isAbsolute(replaced) ? path.normalize(replaced) : path.normalize(path.join(workspaceRoot, replaced));
    }
    getWorkspaceRoot() {
        const folder = vscode.workspace.workspaceFolders?.[0];
        if (!folder) {
            throw new Error("Open a workspace folder before using Renium.");
        }
        return folder.uri.fsPath;
    }
    isPathInside(filePath, rootPath) {
        const relative = path.relative(this.normalizePathForCompare(rootPath), this.normalizePathForCompare(filePath));
        return relative === "" || (!!relative && !relative.startsWith("..") && !path.isAbsolute(relative));
    }
    normalizePathForCompare(filePath) {
        const normalized = path.resolve(filePath);
        return process.platform === "win32" ? normalized.toLowerCase() : normalized;
    }
    editorChangedPathArg(filePath, projectRoot) {
        if (!this.isPathInside(filePath, projectRoot)) {
            return filePath;
        }
        return path.relative(projectRoot, filePath);
    }
    detectServiceForPath(filePath, projectRoot, services) {
        const srcRoot = path.join(projectRoot, "src");
        if (!this.isPathInside(filePath, srcRoot)) {
            return undefined;
        }
        const relative = path.relative(srcRoot, filePath);
        if (relative.startsWith("..") || path.isAbsolute(relative)) {
            return undefined;
        }
        const firstSegment = relative.split(path.sep)[0];
        const byLower = new Map(services.map((s) => [s.toLowerCase(), s]));
        return byLower.get(firstSegment.toLowerCase());
    }
    parseBenchmarkMetrics(output) {
        const lines = output.replace(/\r\n/g, "\n").split("\n");
        const serviceMetrics = new Map();
        const metricsForService = (service) => {
            let metrics = serviceMetrics.get(service);
            if (!metrics) {
                metrics = {
                    pluginServerMs: 0,
                    pluginEncodeMs: 0,
                    payloadBytes: 0,
                    chunkCount: 0,
                    sawPerfLine: false,
                    stallCountOver33Ms: 0,
                    stallCountOver50Ms: 0,
                    stallCountOver100Ms: 0,
                };
                serviceMetrics.set(service, metrics);
            }
            return metrics;
        };
        let runTimingSummary = {};
        for (const line of lines) {
            const payloadMatch = /([A-Za-z][A-Za-z0-9_]*): (?:(?:adaptive wave \d+)|instance) payloads chunk metrics -> chunks=(\d+), bytes=(\d+), .*plugin_server_ms=([0-9.]+), plugin_encode_ms=([0-9.]+)/.exec(line);
            if (payloadMatch) {
                const metrics = metricsForService(payloadMatch[1]);
                metrics.chunkCount += Number.parseInt(payloadMatch[2], 10);
                metrics.payloadBytes += Number.parseInt(payloadMatch[3], 10);
                metrics.pluginServerMs += Number.parseFloat(payloadMatch[4]);
                metrics.pluginEncodeMs += Number.parseFloat(payloadMatch[5]);
            }
            const perfMatch = /([A-Za-z][A-Za-z0-9_]*): adaptive wave \d+ perf stats -> last_frame_ms=([^,]+), max_frame_ms=([^,]+), stalls33=([^,]+), stalls50=([^,]+), stalls100=([^,]+)/.exec(line);
            if (perfMatch) {
                const metrics = metricsForService(perfMatch[1]);
                metrics.sawPerfLine = true;
                const maxFrameMs = Number.parseFloat(perfMatch[3]);
                if (Number.isFinite(maxFrameMs)) {
                    metrics.maxFrameMs = metrics.maxFrameMs === undefined ? maxFrameMs : Math.max(metrics.maxFrameMs, maxFrameMs);
                }
                const stalls33 = Number.parseInt(perfMatch[4], 10);
                if (Number.isFinite(stalls33)) {
                    metrics.stallCountOver33Ms += stalls33;
                }
                const stalls50 = Number.parseInt(perfMatch[5], 10);
                if (Number.isFinite(stalls50)) {
                    metrics.stallCountOver50Ms += stalls50;
                }
                const stalls100 = Number.parseInt(perfMatch[6], 10);
                if (Number.isFinite(stalls100)) {
                    metrics.stallCountOver100Ms += stalls100;
                }
            }
            const instanceFetchMatch = /timing: ([A-Za-z][A-Za-z0-9_]*): instance fetch took ([0-9.]+)ms/.exec(line);
            if (instanceFetchMatch) {
                metricsForService(instanceFetchMatch[1]).instanceFetchMs = Number.parseFloat(instanceFetchMatch[2]);
            }
            const runTimingMatch = /run timing summary: total_ms=([0-9.]+), core_export_ms=([0-9.]+), bridge_startup_ms=([0-9.]+), handshake_ms=([0-9.]+), service_export_sum_ms=([0-9.]+), import_critical_tail_ms=([0-9.]+), unmeasured_or_scheduler_gap_ms=([0-9.]+)/.exec(line);
            if (runTimingMatch) {
                runTimingSummary = {
                    totalMs: Number.parseFloat(runTimingMatch[1]),
                    coreExportMs: Number.parseFloat(runTimingMatch[2]),
                    bridgeStartupMs: Number.parseFloat(runTimingMatch[3]),
                    handshakeMs: Number.parseFloat(runTimingMatch[4]),
                    serviceExportSumMs: Number.parseFloat(runTimingMatch[5]),
                    importCriticalTailMs: Number.parseFloat(runTimingMatch[6]),
                    unmeasuredOrSchedulerGapMs: Number.parseFloat(runTimingMatch[7]),
                };
            }
        }
        let trackedService;
        let trackedMetrics;
        let bestScore = -1;
        for (const [service, metrics] of serviceMetrics.entries()) {
            const score = (metrics.instanceFetchMs ?? 0) * 1000000 +
                metrics.pluginServerMs * 10000 +
                metrics.pluginEncodeMs * 1000 +
                metrics.payloadBytes;
            if (score > bestScore) {
                bestScore = score;
                trackedService = service;
                trackedMetrics = metrics;
            }
        }
        const serviceMetricList = Array.from(serviceMetrics.entries())
            .map(([service, metrics]) => ({
            service,
            instanceFetchMs: metrics.instanceFetchMs,
            pluginServerMs: metrics.chunkCount > 0 ? metrics.pluginServerMs : undefined,
            pluginEncodeMs: metrics.chunkCount > 0 ? metrics.pluginEncodeMs : undefined,
            payloadBytes: metrics.chunkCount > 0 ? metrics.payloadBytes : undefined,
            chunkCount: metrics.chunkCount > 0 ? metrics.chunkCount : undefined,
            maxFrameMs: metrics.maxFrameMs,
            stallCountOver33Ms: metrics.sawPerfLine ? metrics.stallCountOver33Ms : undefined,
            stallCountOver50Ms: metrics.sawPerfLine ? metrics.stallCountOver50Ms : undefined,
            stallCountOver100Ms: metrics.sawPerfLine ? metrics.stallCountOver100Ms : undefined,
        }))
            .sort((a, b) => this.benchmarkServiceScore(b) - this.benchmarkServiceScore(a));
        return {
            totalMs: runTimingSummary.totalMs ?? this.matchLastNumber(output, /full export-snapshots run took ([0-9.]+)ms/g),
            trackedService,
            coreExportMs: runTimingSummary.coreExportMs,
            bridgeStartupMs: runTimingSummary.bridgeStartupMs,
            handshakeMs: runTimingSummary.handshakeMs,
            serviceExportSumMs: runTimingSummary.serviceExportSumMs,
            importCriticalTailMs: runTimingSummary.importCriticalTailMs,
            unmeasuredOrSchedulerGapMs: runTimingSummary.unmeasuredOrSchedulerGapMs,
            trackedServiceInstanceFetchMs: trackedMetrics?.instanceFetchMs,
            trackedServicePluginServerMs: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.pluginServerMs : undefined,
            trackedServicePluginEncodeMs: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.pluginEncodeMs : undefined,
            trackedServicePayloadBytes: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.payloadBytes : undefined,
            trackedServiceChunkCount: trackedMetrics && trackedMetrics.chunkCount > 0 ? trackedMetrics.chunkCount : undefined,
            trackedServiceMaxFrameMs: trackedMetrics?.maxFrameMs,
            trackedServiceStallCountOver33Ms: trackedMetrics?.sawPerfLine ? trackedMetrics.stallCountOver33Ms : undefined,
            trackedServiceStallCountOver50Ms: trackedMetrics?.sawPerfLine ? trackedMetrics.stallCountOver50Ms : undefined,
            trackedServiceStallCountOver100Ms: trackedMetrics?.sawPerfLine ? trackedMetrics.stallCountOver100Ms : undefined,
            exportFingerprint: this.matchLastString(output, /export start: version=([^,\n]+), git=([^,\n]+), build_ts=([^,\n]+), features=([^,\n]+), protocol=([^,\n]+)/g, (match) => `version=${match[1]}, git=${match[2]}, build_ts=${match[3]}, features=${match[4]}, protocol=${match[5]}`),
            bridgeFingerprint: this.matchLastString(output, /bridge info: version=([^,\n]+), build_unix=([^,\n]+), protocol=([^,\n]+), codec=([^,\n]+), chunk_frame=([^,\n]+), compact_value=([^,\n]+), warm_mode=([^,\n]+), serializer_mode=([^,\n]+)/g, (match) => `version=${match[1]}, build_unix=${match[2]}, protocol=${match[3]}, codec=${match[4]}, chunk_frame=${match[5]}, compact_value=${match[6]}, warm_mode=${match[7]}, serializer_mode=${match[8]}`),
            serviceMetrics: serviceMetricList,
        };
    }
    extractPluginProfile(output) {
        const marker = "[renium] plugin op profile";
        const markerIndex = output.lastIndexOf(marker);
        const jsonStart = output.indexOf("{", markerIndex >= 0 ? markerIndex : 0);
        const jsonEnd = output.lastIndexOf("}");
        if (jsonStart < 0 || jsonEnd <= jsonStart) {
            throw new Error("Plugin profile JSON was not found in CLI output.");
        }
        const rawJson = output.slice(jsonStart, jsonEnd + 1);
        let parsed;
        try {
            parsed = JSON.parse(rawJson);
        }
        catch (error) {
            throw new Error(`Failed to parse plugin profile JSON: ${error instanceof Error ? error.message : String(error)}`);
        }
        if (!parsed || typeof parsed !== "object") {
            throw new Error("Plugin profile JSON did not decode to an object.");
        }
        return parsed;
    }
    formatPluginProfileRanking(profile, limit) {
        const projectedCalls = typeof profile.profile?.projectedServerStoragePropertyReads === "number" &&
            Number.isFinite(profile.profile.projectedServerStoragePropertyReads)
            ? profile.profile.projectedServerStoragePropertyReads
            : 1259770;
        const entries = [];
        for (const [name, operation] of Object.entries(profile.operations ?? {})) {
            const perCallUs = typeof operation?.perCallUs === "number" && Number.isFinite(operation.perCallUs)
                ? operation.perCallUs
                : undefined;
            if (perCallUs === undefined) {
                continue;
            }
            entries.push({
                name,
                perCallUs,
                p90Us: typeof operation?.p90Us === "number" && Number.isFinite(operation.p90Us) ? operation.p90Us : undefined,
                projectedMsPer100k: perCallUs * 100,
                projectedServerStorageMs: (perCallUs * projectedCalls) / 1000,
            });
        }
        entries.sort((a, b) => b.projectedMsPer100k - a.projectedMsPer100k);
        const ranked = entries.slice(0, Math.max(1, limit));
        if (ranked.length === 0) {
            return ["[renium] profile: no per-call operations were available to rank."];
        }
        return ranked.map((entry, index) => `[renium] profile: ${String(index + 1).padStart(2, "0")} ${entry.name} per_call=${entry.perCallUs.toFixed(3)}us p90=${entry.p90Us?.toFixed(1) ?? "n/a"}us per_100k=${entry.projectedMsPer100k.toFixed(1)}ms projected_serverstorage=${entry.projectedServerStorageMs.toFixed(1)}ms`);
    }
    matchLastNumber(output, pattern) {
        const match = this.matchLastString(output, pattern);
        if (!match) {
            return undefined;
        }
        const value = Number.parseFloat(match);
        return Number.isFinite(value) ? value : undefined;
    }
    matchLastString(output, pattern, formatter) {
        let matched;
        let result;
        pattern.lastIndex = 0;
        while ((result = pattern.exec(output)) !== null) {
            matched = formatter ? formatter(result) : result[1];
        }
        return matched;
    }
    percentile(values, percentile) {
        const filtered = values.filter((value) => value !== undefined && Number.isFinite(value)).sort((a, b) => a - b);
        if (filtered.length === 0) {
            return undefined;
        }
        const rank = Math.max(0, Math.ceil(percentile * filtered.length) - 1);
        return filtered[Math.min(filtered.length - 1, rank)];
    }
    minMetric(values) {
        const filtered = values.filter((value) => value !== undefined && Number.isFinite(value));
        return filtered.length > 0 ? Math.min(...filtered) : undefined;
    }
    maxMetric(values) {
        const filtered = values.filter((value) => value !== undefined && Number.isFinite(value));
        return filtered.length > 0 ? Math.max(...filtered) : undefined;
    }
    buildBenchmarkSummary(runs) {
        const lastRun = runs[runs.length - 1];
        return {
            totalMs: this.benchmarkMetricSummary(runs.map((run) => run.totalMs)),
            trackedServiceInstanceFetchMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceInstanceFetchMs)),
            trackedServicePluginServerMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServicePluginServerMs)),
            trackedServicePluginEncodeMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServicePluginEncodeMs)),
            trackedServicePayloadBytes: this.benchmarkMetricSummary(runs.map((run) => run.trackedServicePayloadBytes)),
            trackedServiceChunkCount: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceChunkCount)),
            trackedServiceMaxFrameMs: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceMaxFrameMs)),
            trackedServiceStallCountOver50Ms: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceStallCountOver50Ms)),
            trackedServiceStallCountOver100Ms: this.benchmarkMetricSummary(runs.map((run) => run.trackedServiceStallCountOver100Ms)),
            coreExportMs: this.benchmarkMetricSummary(runs.map((run) => run.coreExportMs)),
            bridgeStartupMs: this.benchmarkMetricSummary(runs.map((run) => run.bridgeStartupMs)),
            handshakeMs: this.benchmarkMetricSummary(runs.map((run) => run.handshakeMs)),
            serviceExportSumMs: this.benchmarkMetricSummary(runs.map((run) => run.serviceExportSumMs)),
            importCriticalTailMs: this.benchmarkMetricSummary(runs.map((run) => run.importCriticalTailMs)),
            unmeasuredOrSchedulerGapMs: this.benchmarkMetricSummary(runs.map((run) => run.unmeasuredOrSchedulerGapMs)),
            perService: this.benchmarkPerServiceSummary(runs),
            exportFingerprint: lastRun?.exportFingerprint,
            bridgeFingerprint: lastRun?.bridgeFingerprint,
        };
    }
    benchmarkMetricSummary(values) {
        return {
            p50: this.percentile(values, 0.5),
            p90: this.percentile(values, 0.9),
            min: this.minMetric(values),
            max: this.maxMetric(values),
        };
    }
    benchmarkPerServiceSummary(runs) {
        const byService = new Map();
        for (const run of runs) {
            for (const metrics of run.serviceMetrics ?? []) {
                const serviceRuns = byService.get(metrics.service) ?? [];
                serviceRuns.push(metrics);
                byService.set(metrics.service, serviceRuns);
            }
        }
        const entries = Array.from(byService.entries()).sort((a, b) => {
            const aP50 = this.percentile(a[1].map((metrics) => metrics.instanceFetchMs), 0.5) ?? 0;
            const bP50 = this.percentile(b[1].map((metrics) => metrics.instanceFetchMs), 0.5) ?? 0;
            return bP50 - aP50;
        });
        const out = {};
        for (const [service, serviceRuns] of entries) {
            out[service] = {
                instanceFetchMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.instanceFetchMs)),
                pluginServerMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.pluginServerMs)),
                pluginEncodeMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.pluginEncodeMs)),
                payloadBytes: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.payloadBytes)),
                chunkCount: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.chunkCount)),
                maxFrameMs: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.maxFrameMs)),
                stallCountOver33Ms: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.stallCountOver33Ms)),
                stallCountOver50Ms: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.stallCountOver50Ms)),
                stallCountOver100Ms: this.benchmarkMetricSummary(serviceRuns.map((metrics) => metrics.stallCountOver100Ms)),
            };
        }
        return out;
    }
    benchmarkServiceScore(metrics) {
        return ((metrics.instanceFetchMs ?? 0) * 1000000 +
            (metrics.pluginServerMs ?? 0) * 10000 +
            (metrics.pluginEncodeMs ?? 0) * 1000 +
            (metrics.payloadBytes ?? 0));
    }
    logBenchmarkRun(prefix, metrics) {
        this.output.appendLine(`${prefix} total=${this.formatMetricMs(metrics.totalMs)} core_export=${this.formatMetricMs(metrics.coreExportMs)} bridge_startup=${this.formatMetricMs(metrics.bridgeStartupMs)} handshake=${this.formatMetricMs(metrics.handshakeMs)} service_export_sum=${this.formatMetricMs(metrics.serviceExportSumMs)} import_tail=${this.formatMetricMs(metrics.importCriticalTailMs)} gap=${this.formatMetricMs(metrics.unmeasuredOrSchedulerGapMs)} trackedService=${metrics.trackedService ?? "n/a"} fetch=${this.formatMetricMs(metrics.trackedServiceInstanceFetchMs)} pluginServer=${this.formatMetricMs(metrics.trackedServicePluginServerMs)} pluginEncode=${this.formatMetricMs(metrics.trackedServicePluginEncodeMs)} payload=${this.formatMetricBytes(metrics.trackedServicePayloadBytes)} chunks=${this.formatMetricInt(metrics.trackedServiceChunkCount)} maxFrame=${this.formatMetricMs(metrics.trackedServiceMaxFrameMs)} stalls50=${this.formatMetricInt(metrics.trackedServiceStallCountOver50Ms)} stalls100=${this.formatMetricInt(metrics.trackedServiceStallCountOver100Ms)}`);
    }
    summaryP50(summary, key) {
        const metricSummary = summary?.[key];
        if (!metricSummary || typeof metricSummary !== "object" || !("p50" in metricSummary)) {
            return undefined;
        }
        const value = metricSummary.p50;
        return typeof value === "number" && Number.isFinite(value) ? value : undefined;
    }
    metricDelta(before, after) {
        return before === undefined || after === undefined ? undefined : after - before;
    }
    formatMetricMs(value) {
        return value === undefined ? "n/a" : `${value.toFixed(1)}ms`;
    }
    formatSignedMetricMs(value) {
        if (value === undefined) {
            return "n/a";
        }
        return `${value >= 0 ? "+" : ""}${value.toFixed(1)}ms`;
    }
    formatMetricBytes(value) {
        return value === undefined ? "n/a" : `${Math.round(value)}B`;
    }
    formatMetricInt(value) {
        return value === undefined ? "n/a" : String(Math.round(value));
    }
    summarizeBenchmarkOutput(output) {
        const lines = output
            .replace(/\r\n/g, "\n")
            .split("\n")
            .map((line) => line.trim())
            .filter((line) => line.length > 0);
        const interesting = lines.filter((line) => line.includes("effective chunk size:") ||
            line.includes("prepared bridge_version=") ||
            line.includes("instance fetch") ||
            line.includes("script source fetch") ||
            line.includes("build service state") ||
            line.includes("settings binary collect") ||
            line.includes("settings binary write") ||
            line.includes("direct import worker total") ||
            line.includes("direct import dispatcher drain") ||
            line.includes("export start:"));
        return interesting.slice(-16);
    }
    renderArgs(args) {
        return args
            .map((arg) => {
            if (/\s/.test(arg) || arg.includes('"')) {
                return `"${arg.replaceAll('"', '\\"')}"`;
            }
            return arg;
        })
            .join(" ");
    }
    updateStatusBar() {
        if (this.activeTaskName) {
            const elapsedSeconds = Math.max(0, Math.floor((Date.now() - this.activeTaskStartedAt) / 1000));
            this.statusItem.text = `$(sync~spin) Renium ${elapsedSeconds}s`;
            this.statusItem.tooltip = `${this.activeTaskName} in progress`;
            return;
        }
        const config = vscode.workspace.getConfiguration("renium");
        const autoEnabled = config.get("autoSyncOnSave", false);
        const liveSyncEnabled = this.editorLiveSyncRuntimeEnabled;
        if (this.bridgeServeRequested && this.isBridgeDaemonRunning()) {
            this.statusItem.text = "$(radio-tower) Renium Serve";
            this.statusItem.tooltip = "Bridge server is running; Studio plugin can connect";
            return;
        }
        if (liveSyncEnabled && this.liveSyncWatcher) {
            this.statusItem.text = "$(sync~spin) Renium Live";
            this.statusItem.tooltip = "Live sync running";
            return;
        }
        if (autoEnabled) {
            this.statusItem.text = "$(sync) Renium Auto";
            this.statusItem.tooltip = "Auto sync on save is enabled";
            return;
        }
        this.statusItem.text = "$(sync) Renium";
        this.statusItem.tooltip = "Open Renium menu";
    }
    setActiveTask(taskName) {
        this.activeTaskName = taskName;
        this.activeTaskStartedAt = taskName ? Date.now() : 0;
        if (this.activeTaskTicker) {
            clearInterval(this.activeTaskTicker);
            this.activeTaskTicker = undefined;
        }
        if (taskName) {
            this.activeTaskTicker = setInterval(() => {
                this.updateStatusBar();
            }, 1000);
        }
        this.updateStatusBar();
    }
    disposeLiveSyncRuntime() {
        this.stopStudioLiveSyncRuntime();
        if (this.liveSyncWatcher) {
            this.liveSyncWatcher.dispose();
            this.liveSyncWatcher = undefined;
        }
        if (this.liveSyncTimer) {
            clearTimeout(this.liveSyncTimer);
            this.liveSyncTimer = undefined;
        }
        this.pendingEditorPaths.clear();
        this.recentDirectSaveAtByPath.clear();
    }
    async setEditorLiveSyncEnabled(enabled) {
        this.editorLiveSyncRuntimeEnabled = enabled;
        this.updateStatusBar();
    }
}
function activate(context) {
    const controller = new RobloxSyncController(context);
    const fileExplorerController = new fileExplorer_1.FileExplorerController(context);
    context.subscriptions.push(controller, fileExplorerController, vscode.commands.registerCommand("renium.openMenu", () => controller.openMenu()), vscode.commands.registerCommand("renium.openExplorer", () => vscode.commands.executeCommand("workbench.view.extension.reniumContainer")), vscode.commands.registerCommand("renium.fullSync", () => controller.fullSync()), vscode.commands.registerCommand("renium.benchmarkFullSync", () => controller.benchmarkFullSync()), vscode.commands.registerCommand("renium.benchmarkModifiedDefaultBypassAB", () => controller.benchmarkModifiedDefaultBypassAB()), vscode.commands.registerCommand("renium.profilePluginOps", () => controller.profilePluginOperations()), vscode.commands.registerCommand("renium.exportSnapshots", () => controller.exportSnapshotsOnly()), vscode.commands.registerCommand("renium.importSnapshots", () => controller.importSnapshotsOnly()), vscode.commands.registerCommand("renium.startLiveSync", () => controller.startLiveSync()), vscode.commands.registerCommand("renium.stopLiveSync", () => controller.stopLiveSync()), vscode.commands.registerCommand("renium.retryEditorInitialSync", () => controller.retryEditorInitialSync()), vscode.commands.registerCommand("renium.serve", () => controller.serve()), vscode.commands.registerCommand("renium.stopServe", () => controller.stopServe()), vscode.commands.registerCommand("renium.syncActiveService", () => controller.syncActiveService()), vscode.commands.registerCommand("renium.pushEditorPathsNow", (paths, options) => controller.pushEditorPathsNow(paths, options)), vscode.commands.registerCommand("renium.pushEditorPropertyNow", (request) => controller.pushEditorPropertyNow(request)), vscode.commands.registerCommand("renium.pushEditorDeleteNow", (request) => controller.pushEditorDeleteNow(request)), vscode.workspace.onDidSaveTextDocument((doc) => {
        void controller.onDocumentSaved(doc);
    }), vscode.workspace.onDidChangeConfiguration((event) => {
        if (event.affectsConfiguration("renium")) {
            controller.onConfigurationChanged(event);
        }
    }));
}
function deactivate() {

}
