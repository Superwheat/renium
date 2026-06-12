import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const GENERATED_FILE_NAME = "roblox-properties.generated.json";
const GENERATED_STUDIO_API_SCHEMA_FILE_NAME = "BridgeStudioApiSchema.module.lua";
const GENERATED_CLASS_LIST_FILE_NAME = "robloxClasses.ts";
const BLOCKED_TAGS = new Set(["ReadOnly", "Hidden", "Deprecated", "NotScriptable", "NotBrowsable", "WriteOnly"]);
const ENGINE_MANAGED_TYPES = new Set(["UniqueId", "SecurityCapabilities"]);
const ALLOWED_WRITE_SECURITY = new Set(["None", "PluginSecurity"]);
const MODEL_PIVOT_CLASSES = new Set(["Model", "WorldModel", "Workspace"]);
const LIGHTING_HIDDEN_STUDIO_PROPERTIES = new Set([
  "ExtendLightRangeTo120",
]);
const WORKSPACE_HIDDEN_STUDIO_PROPERTIES = new Set([
  "AirTurbulenceIntensity",
  "CurrentCamera",
  "LevelOfDetail",
  "ModelStreamingMode",
  "Origin",
  "Pivot Offset",
  "Scale",
  "StreamingEnabledAlias",
]);
const WORKSPACE_VISIBLE_NON_SERIALIZED_PROPERTIES = new Set(["InsertPoint"]);
const WORKSPACE_VISIBLE_SERVICE_REF_PROPERTIES = new Set(["PrimaryPart"]);
const WORKSPACE_SERVER_AUTHORITY_PROPERTIES = new Set([
  "AuthorityMode",
  "NextGenerationReplication",
  "PlayerScriptsUseInputActionSystem",
  "SignalBehavior",
  "UseFixedSimulation",
]);
const CURRENT_WORKSPACE_API_PROPERTIES = {
  NextGenerationReplication: { type: "Enum.RolloutState", category: "Server Authority", order: 210 },
  PlayerScriptsUseInputActionSystem: { type: "Enum.RolloutState", category: "Server Authority", order: 230 },
  UseFixedSimulation: { type: "Enum.RolloutState", category: "Server Authority", order: 250 },
};
const FORCED_STUDIO_API_PROPERTIES = {
  MaterialService: {
    Use2022Materials: {
      type: "Bool",
      category: "Material Pack",
      order: 10,
    },
  },
};
const STARTER_PLAYER_CHARACTER_PROPERTIES = new Set([
  "AvatarJointUpgrade_SerializedRollout",
  "CharacterMaxSlopeAngle",
  "CharacterWalkSpeed",
  "LoadCharacterAppearance",
  "LoadCharacterLayeredClothing",
  "UserEmotesEnabled",
]);
const CATEGORY_LABELS = {
  accessories: "Accessories",
  advanced: "Advanced",
  airproperties: "Air Properties",
  alignment: "Alignment",
  alignmentmode: "Alignment Mode",
  angularlimits: "Angular Limits",
  angularmotor: "Angular Motor",
  angularservo: "Angular Servo",
  animatable: "Animatable",
  animation: "Animation",
  appearance: "Appearance",
  assembly: "Assembly",
  asset: "Asset",
  attachments: "Attachments",
  audio: "Audio",
  authentication: "Authentication",
  avatar: "Avatar",
  "auto-recovery": "Auto-Recovery",
  "auto-save": "Auto-Save",
  axes: "Axes",
  balance: "Balance",
  ballsocket: "Ball Socket",
  behavior: "Behavior",
  benchmarking: "Benchmarking",
  bodycolors: "Body Colors",
  bodydata: "Body Data",
  bodyparts: "Body Parts",
  browsing: "Browsing",
  cache: "Cache",
  camera: "Camera",
  character: "Character",
  characterjumpsettings: "Character Jump Settings",
  clothes: "Clothes",
  collision: "Collision",
  compliance: "Compliance",
  compositedirections: "Composite Directions",
  configuration: "Configuration",
  connections: "Connections",
  control: "Control",
  controlpoints: "Control Points",
  controls: "Controls",
  cylinder: "Cylinder",
  data: "Data",
  debug: "Debug",
  debugging: "Debugging",
  derived: "Derived",
  deriveddata: "Derived Data",
  derivedworlddata: "Derived World Data",
  destruction: "Destruction",
  devicedeployment: "Device Deployment",
  diagnostics: "Diagnostics",
  directories: "Directories",
  display: "Display",
  dragdirections: "Drag Directions",
  draggedamount: "Dragged Amount",
  draglimits: "Drag Limits",
  emission: "Emission",
  emitter: "Emitter",
  emittershape: "Emitter Shape",
  errors: "Errors",
  explorer: "Explorer",
  exposure: "Exposure",
  flipbook: "Flipbook",
  fog: "Fog",
  forcefield: "Force Field",
  friction: "Friction",
  game: "Game",
  garbagecollection: "Garbage Collection",
  general: "General",
  goals: "Goals",
  hinge: "Hinge",
  image: "Image",
  input: "Input",
  inserts: "Inserts",
  instance: "Instance",
  joint: "Joint",
  layout: "Layout",
  limits: "Limits",
  linear: "Linear",
  linearmotor: "Linear Motor",
  linearservo: "Linear Servo",
  localization: "Localization",
  material: "Material",
  mesh: "Mesh",
  meshes: "Meshes",
  motion: "Motion",
  network: "Network",
  networking: "Networking",
  output: "Output",
  part: "Part",
  pathfinding: "Pathfinding",
  physics: "Physics",
  pivot: "Pivot",
  playback: "Playback",
  rendering: "Rendering",
  scale: "Scale",
  script: "Script",
  scripting: "Scripting",
  scrolling: "Scrolling",
  security: "Security",
  selection: "Selection",
  shape: "Shape",
  serverauthority: "Server Authority",
  sound: "Sound",
  sounds: "Sounds",
  state: "State",
  streaming: "Streaming",
  surface: "Surface",
  terrain: "Terrain",
  text: "Text",
  transform: "Transform",
  tween: "Tween",
  userinterface: "User Interface",
  value: "Value",
  values: "Values",
  vector: "Vector",
  video: "Video",
  visualization: "Visualization",
  winch: "Winch",
};

function safeObject(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function safeArray(value) {
  return Array.isArray(value) ? value : [];
}

function normalizeKey(text) {
  return String(text ?? "").toLowerCase().replace(/[\s_]+/g, "");
}

function decodeXmlText(text) {
  return String(text ?? "")
    .replace(/&quot;/g, "\"")
    .replace(/&apos;/g, "'")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&amp;/g, "&");
}

function titleCase(text) {
  return text
    .split(/([\s-]+)/)
    .map((part) => /^[a-z]/i.test(part) ? part[0].toUpperCase() + part.slice(1).toLowerCase() : part)
    .join("");
}

function categoryLabel(category) {
  const key = normalizeKey(category);
  return CATEGORY_LABELS[key] ?? titleCase(String(category ?? "Data").replace(/-/g, " "));
}

function rbxDomDataTypeFromApiDumpValueType(name, category) {
  if (!name) {
    return undefined;
  }
  if (category === "Enum") {
    return { Enum: name.replace(/^Enum\./, "") };
  }
  if (category === "Class") {
    return { Value: "Ref" };
  }
  const primitiveMap = {
    bool: "Bool",
    boolean: "Bool",
    int: "Int32",
    int64: "Int64",
    float: "Float32",
    double: "Float64",
    string: "String",
    BinaryString: "BinaryString",
    Content: "ContentId",
  };
  return { Value: primitiveMap[name] ?? name };
}

function normalizePropertySource(raw) {
  const record = safeObject(raw);
  if (!Array.isArray(record.Classes)) {
    const enums = safeObject(record.Enums);
    const classes = {};
    for (const [className, rawClass] of Object.entries(safeObject(record.Classes))) {
      const classRecord = safeObject(rawClass);
      const properties = {};
      for (const [propertyName, rawProperty] of Object.entries(safeObject(classRecord.Properties))) {
        const property = safeObject(rawProperty);
        properties[propertyName] = {
          ...property,
          Name: String(property.Name ?? propertyName),
          Tags: safeArray(property.Tags).map(String),
        };
      }
      classes[className] = {
        Name: String(classRecord.Name ?? className),
        Superclass: typeof classRecord.Superclass === "string" ? classRecord.Superclass : undefined,
        Tags: safeArray(classRecord.Tags).map(String),
        Properties: properties,
        DefaultProperties: safeObject(classRecord.DefaultProperties),
      };
    }
    return { classes, enums, sourceKind: "rbx-dom" };
  }

  const classes = {};
  for (const rawClass of record.Classes) {
    const classRecord = safeObject(rawClass);
    const className = String(classRecord.Name ?? "");
    if (!className) {
      continue;
    }
    const properties = {};
    for (const rawMember of safeArray(classRecord.Members)) {
      const member = safeObject(rawMember);
      if (member.MemberType !== "Property") {
        continue;
      }
      const propertyName = String(member.Name ?? "");
      if (!propertyName) {
        continue;
      }
      const valueType = safeObject(member.ValueType);
      const valueTypeName = typeof valueType.Name === "string" ? valueType.Name : undefined;
      const valueTypeCategory = typeof valueType.Category === "string" ? valueType.Category : undefined;
      properties[propertyName] = {
        Name: propertyName,
        MemberType: "Property",
        Security: safeObject(member.Security),
        Scriptability: typeof member.Scriptability === "string" ? member.Scriptability : undefined,
        SourceKind: "api-dump",
        ValueType: { Name: valueTypeName, Category: valueTypeCategory },
        DataType: rbxDomDataTypeFromApiDumpValueType(valueTypeName, valueTypeCategory),
        Category: typeof member.Category === "string" ? member.Category : undefined,
        Tags: safeArray(member.Tags).map(String),
      };
    }
    classes[className] = {
      Name: className,
      Superclass: typeof classRecord.Superclass === "string" ? classRecord.Superclass : undefined,
      Tags: safeArray(classRecord.Tags).map(String),
      Properties: properties,
    };
  }
  const enums = {};
  for (const rawEnum of safeArray(record.Enums)) {
    const enumRecord = safeObject(rawEnum);
    const enumName = String(enumRecord.Name ?? "");
    if (!enumName) {
      continue;
    }
    const items = {};
    for (const rawItem of safeArray(enumRecord.Items)) {
      const item = safeObject(rawItem);
      const itemName = String(item.Name ?? "");
      const itemValue = Number(item.Value);
      if (itemName && Number.isFinite(itemValue)) {
        items[itemName] = itemValue;
      }
    }
    enums[enumName] = { items };
  }
  return { classes, enums, sourceKind: "api-dump" };
}

function findLatestStudioExecutable() {
  const localAppData = process.env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local");
  const versionsRoot = path.join(localAppData, "Roblox", "Versions");
  if (!fs.existsSync(versionsRoot)) {
    return undefined;
  }
  const candidates = [];
  for (const entry of fs.readdirSync(versionsRoot, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const candidate = path.join(versionsRoot, entry.name, "RobloxStudioBeta.exe");
    if (!fs.existsSync(candidate)) {
      continue;
    }
    const stat = fs.statSync(candidate);
    candidates.push({ path: candidate, mtimeMs: stat.mtimeMs });
  }
  candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return candidates[0];
}

function waitForFile(pathToWaitFor, minBytes, timeoutMs) {
  const started = Date.now();
  const sleepBuffer = new SharedArrayBuffer(4);
  const sleepView = new Int32Array(sleepBuffer);
  while (Date.now() - started < timeoutMs) {
    if (fs.existsSync(pathToWaitFor) && fs.statSync(pathToWaitFor).size >= minBytes) {
      return true;
    }
    Atomics.wait(sleepView, 0, 0, 100);
  }
  return fs.existsSync(pathToWaitFor) && fs.statSync(pathToWaitFor).size >= minBytes;
}

function ensureStudioApiDump(repoRoot) {
  const outputPath = path.join(repoRoot, "tools", "API-Dump.json");
  const studio = findLatestStudioExecutable();
  if (!studio) {
    return fs.existsSync(outputPath) ? outputPath : undefined;
  }

  const outputStat = fs.existsSync(outputPath) ? fs.statSync(outputPath) : undefined;
  if (outputStat && outputStat.size > 1024 * 1024 && outputStat.mtimeMs >= studio.mtimeMs) {
    return outputPath;
  }

  fs.mkdirSync(path.dirname(outputPath), { recursive: true });
  spawnSync(studio.path, ["-API", outputPath], {
    cwd: path.dirname(outputPath),
    stdio: "ignore",
    windowsHide: true,
    timeout: 120000,
  });
  return waitForFile(outputPath, 1024 * 1024, 30000) ? outputPath : outputStat ? outputPath : undefined;
}

function resolvePropertySource(repoRoot) {
  const studioApiDump = ensureStudioApiDump(repoRoot);
  return [
    studioApiDump,
    path.join(repoRoot, "API-Dump.json"),
    path.join(repoRoot, "tools", "API-Dump.json"),
    path.join(repoRoot, "Full-API-Dump.json"),
    path.join(repoRoot, "tools", "plugin_ws_bridge", "rbx_dom_lua", "database.json"),
  ].find((candidate) => candidate && fs.existsSync(candidate));
}

function propertyDataType(property) {
  if (property?.DataType?.Enum) {
    return `Enum.${property.DataType.Enum}`;
  }
  if (property?.DataType?.Value) {
    return property.DataType.Value;
  }
  if (property?.ValueType?.Category === "Enum" && property.ValueType.Name) {
    return `Enum.${String(property.ValueType.Name).replace(/^Enum\./, "")}`;
  }
  return property?.ValueType?.Name;
}

function enumItemsForDataType(enums, dataType) {
  if (!dataType?.startsWith("Enum.")) {
    return undefined;
  }
  const enumType = dataType.slice("Enum.".length);
  const items = safeObject(safeObject(enums?.[enumType]).items);
  const names = Object.entries(items)
    .filter(([, value]) => Number.isFinite(Number(value)))
    .sort((a, b) => Number(a[1]) - Number(b[1]))
    .map(([name]) => name);
  return names.length > 0 ? names : undefined;
}

function propertyTags(property) {
  return new Set(safeArray(property?.Tags).map(String));
}

function hasBlockedTag(property, options = {}) {
  const allowHidden = options.allowHidden === true;
  const allowNotScriptable = options.allowNotScriptable === true;
  const tags = propertyTags(property);
  for (const tag of BLOCKED_TAGS) {
    if (tag === "Hidden" && allowHidden) {
      continue;
    }
    if (tag === "NotScriptable" && allowNotScriptable) {
      continue;
    }
    if (tags.has(tag)) {
      return true;
    }
  }
  return false;
}

function isAlwaysHiddenPropertyName(name) {
  return new Set([
    "Name",
    "ClassName",
    "Parent",
    "Sandboxed",
    "DefinesCapabilities",
    "Attributes",
    "Tags",
    "Source",
    "LinkedSource",
  ]).has(name);
}

function isSerializedProperty(property) {
  const kind = property?.Kind;
  if (!kind || typeof kind !== "object" || Array.isArray(kind)) {
    return true;
  }
  if (kind.Alias && typeof kind.Alias === "object") {
    return false;
  }
  const canonical = kind.Canonical;
  if (!canonical || typeof canonical !== "object" || Array.isArray(canonical)) {
    return true;
  }
  return canonical.Serialization !== "DoesNotSerialize";
}

function isApiDumpProperty(property) {
  return property?.SourceKind === "api-dump";
}

function classHasTag(classes, className, tag) {
  return safeArray(classes[className]?.Tags).includes(tag);
}

function hasOwnDefaultProperty(classes, className, propertyName) {
  const defaults = safeObject(classes[className]?.DefaultProperties);
  return Object.prototype.hasOwnProperty.call(defaults, propertyName);
}

function isDefaultBackedProperty(classes, className, declaringClass, propertyName) {
  return hasOwnDefaultProperty(classes, className, propertyName) ||
    hasOwnDefaultProperty(classes, declaringClass, propertyName);
}

function allowsHiddenDefaultBackedProperty(propertyName, property) {
  const tags = propertyTags(property);
  if (propertyName === "AvatarJointUpgrade_SerializedRollout") {
    return true;
  }
  const dataType = propertyDataType(property);
  if (!tags.has("Hidden")) {
    return false;
  }
  if (!tags.has("NotScriptable") || tags.has("NotReplicated")) {
    return false;
  }
  if (dataType !== "Enum.LoadCharacterLayeredClothing") {
    return false;
  }
  return !/^GameSettings/i.test(propertyName) &&
    !/Serialized|Rollout/i.test(propertyName);
}

function shouldEmitProperty(className, propertyName, category) {
  if (className === "Lighting" && LIGHTING_HIDDEN_STUDIO_PROPERTIES.has(propertyName)) {
    return false;
  }
  if (className === "Workspace" && WORKSPACE_HIDDEN_STUDIO_PROPERTIES.has(propertyName)) {
    return false;
  }
  if (className === "StarterPlayer" && categoryLabel(category) === "Character") {
    return STARTER_PLAYER_CHARACTER_PROPERTIES.has(propertyName);
  }
  return true;
}

function displayNameOverride(className, propertyName) {
  if (className === "StarterPlayer" && propertyName === "AvatarJointUpgrade_SerializedRollout") {
    return "AvatarJointUpgrade";
  }
  return undefined;
}

function categoryOverride(className, propertyName) {
  if (propertyName === "Archivable") {
    return "Data";
  }
  if (MODEL_PIVOT_CLASSES.has(className) && (propertyName === "PrimaryPart" || propertyName === "WorldPivot")) {
    return "Pivot";
  }
  if (className === "Workspace" && propertyName === "SandboxedInstanceMode") {
    return "Permissions";
  }
  if (className === "Workspace" && WORKSPACE_SERVER_AUTHORITY_PROPERTIES.has(propertyName)) {
    return "Server Authority";
  }
  if (className === "Workspace" && propertyName === "InsertPoint") {
    return "Data";
  }
  if (className === "StarterPlayer" && propertyName === "AvatarJointUpgrade_SerializedRollout") {
    return "Character";
  }
  return undefined;
}

function allowsNonSerializedPropertyForClass(className, propertyName) {
  return (propertyName === "WorldPivot" && MODEL_PIVOT_CLASSES.has(className)) ||
    (className === "Workspace" && WORKSPACE_VISIBLE_NON_SERIALIZED_PROPERTIES.has(propertyName));
}

function allowsServiceRefPropertyForClass(className, propertyName) {
  return className === "Workspace" && WORKSPACE_VISIBLE_SERVICE_REF_PROPERTIES.has(propertyName);
}

function isWritablePropertyForClass(classes, className, declaringClass, propertyName, property) {
  if (!property) {
    return false;
  }
  if (isAlwaysHiddenPropertyName(propertyName)) {
    return false;
  }
  const defaultBacked = isDefaultBackedProperty(classes, className, declaringClass, propertyName);
  if (property.MemberType && property.MemberType !== "Property") {
    return false;
  }
  const writeSecurity = property.Security?.Write;
  const apiDumpStudioWritable = isApiDumpProperty(property) &&
    (writeSecurity === undefined || ALLOWED_WRITE_SECURITY.has(writeSecurity));
  if (hasBlockedTag(property, {
    allowHidden: defaultBacked && allowsHiddenDefaultBackedProperty(propertyName, property),
    allowNotScriptable: defaultBacked || apiDumpStudioWritable,
  })) {
    return false;
  }
  if (!defaultBacked && writeSecurity !== undefined && !ALLOWED_WRITE_SECURITY.has(writeSecurity)) {
    return false;
  }
  if (!defaultBacked && property.Scriptability && property.Scriptability !== "ReadWrite") {
    return false;
  }
  if (!isSerializedProperty(property) && !allowsNonSerializedPropertyForClass(className, propertyName)) {
    return false;
  }
  const dataType = propertyDataType(property);
  if (ENGINE_MANAGED_TYPES.has(dataType)) {
    return false;
  }
  if (dataType === "Ref" && classHasTag(classes, className, "Service") && !allowsServiceRefPropertyForClass(className, propertyName)) {
    return false;
  }
  return true;
}

function collectClassChain(classes, className) {
  const chain = [];
  const seen = new Set();
  let current = className;
  while (current && !seen.has(current)) {
    seen.add(current);
    const classInfo = classes[current];
    if (!classInfo) {
      break;
    }
    chain.unshift(current);
    current = classInfo.Superclass;
  }
  return chain;
}

function directPropertiesFromXml(xml, startIndex) {
  const nextItem = xml.indexOf("<Item", startIndex);
  const propertiesStart = xml.indexOf("<Properties>", startIndex);
  if (propertiesStart === -1 || (nextItem !== -1 && nextItem < propertiesStart)) {
    return {};
  }
  const propertiesEnd = xml.indexOf("</Properties>", propertiesStart);
  if (propertiesEnd === -1) {
    return {};
  }
  const body = xml.slice(propertiesStart + "<Properties>".length, propertiesEnd);
  const properties = {};
  const valuePattern = /<([A-Za-z0-9]+)\s+name="([^"]+)">([\s\S]*?)<\/\1>/g;
  let match;
  while ((match = valuePattern.exec(body))) {
    const [, type, rawName, rawValue] = match;
    const name = decodeXmlText(rawName);
    const valueText = decodeXmlText(rawValue.trim());
    if (type === "bool") {
      properties[name] = valueText.toLowerCase() === "true";
    } else if (type === "int" || type === "float" || type === "double") {
      const numericValue = Number(valueText);
      properties[name] = Number.isFinite(numericValue) ? numericValue : valueText;
    } else {
      properties[name] = valueText;
    }
  }
  return properties;
}

function numericMetadataValue(value) {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === "string" && value.trim()) {
    const numericValue = Number(value);
    return Number.isFinite(numericValue) ? numericValue : undefined;
  }
  return undefined;
}

function parseReflectionMetadata(xml) {
  const classes = {};
  const stack = [];
  const itemPattern = /<Item class="([^"]+)">|<\/Item>/g;
  let match;
  while ((match = itemPattern.exec(xml))) {
    if (match[1]) {
      const item = {
        className: match[1],
        properties: directPropertiesFromXml(xml, itemPattern.lastIndex),
      };
      const parent = stack[stack.length - 1];
      if (item.className === "ReflectionMetadataMember" && parent?.className === "ReflectionMetadataProperties") {
        const owner = [...stack].reverse().find((candidate) => candidate.className === "ReflectionMetadataClass");
        const ownerName = owner?.properties?.Name;
        const memberName = item.properties.Name;
        if (ownerName && memberName) {
          classes[ownerName] ??= {};
          classes[ownerName][memberName] = {
            category: typeof item.properties.Category === "string" ? item.properties.Category : undefined,
            displayName: typeof item.properties.DisplayName === "string" ? item.properties.DisplayName : undefined,
            order: numericOrder(item.properties.PropertyOrder ?? item.properties.EditorOrder ?? item.properties.Order),
            visible: item.properties.Browsable !== false && item.properties.Scriptable !== false,
            uiMinimum: numericMetadataValue(item.properties.UIMinimum),
            uiMaximum: numericMetadataValue(item.properties.UIMaximum),
            uiNumTicks: numericMetadataValue(item.properties.UINumTicks),
            sliderScaling: typeof item.properties.SliderScaling === "string" ? item.properties.SliderScaling : undefined,
          };
        }
      }
      stack.push(item);
    } else {
      stack.pop();
    }
  }
  return { classes };
}

function numericOrder(value) {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === "string" && value.trim()) {
    const numericValue = Number(value);
    return Number.isFinite(numericValue) ? numericValue : undefined;
  }
  return undefined;
}

function findLatestReflectionMetadata(repoRoot) {
  const localCandidates = [
    path.join(repoRoot, "ReflectionMetadata.xml"),
    path.join(repoRoot, "tools", "ReflectionMetadata.xml"),
  ].filter((candidate) => fs.existsSync(candidate));
  if (localCandidates.length > 0) {
    return localCandidates[0];
  }

  const localAppData = process.env.LOCALAPPDATA || path.join(os.homedir(), "AppData", "Local");
  const versionsRoot = path.join(localAppData, "Roblox", "Versions");
  if (!fs.existsSync(versionsRoot)) {
    return undefined;
  }
  const candidates = [];
  for (const entry of fs.readdirSync(versionsRoot, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const candidate = path.join(versionsRoot, entry.name, "ReflectionMetadata.xml");
    if (!fs.existsSync(candidate)) {
      continue;
    }
    const stat = fs.statSync(candidate);
    candidates.push({ path: candidate, mtimeMs: stat.mtimeMs });
  }
  candidates.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return candidates[0]?.path;
}

function loadSorterData(extensionRoot) {
  const sorterPath = path.join(extensionRoot, "resources", "robloxPropertySorters.js");
  if (!fs.existsSync(sorterPath)) {
    return { path: undefined, data: undefined };
  }
  const source = fs.readFileSync(sorterPath, "utf8");
  const marker = "const ORDER_DATA = ";
  const start = source.indexOf(marker);
  const end = source.indexOf("\n};", start);
  if (start === -1 || end === -1) {
    return { path: sorterPath, data: undefined };
  }
  const literal = source.slice(start + marker.length, end + 2);
  return {
    path: sorterPath,
    data: vm.runInNewContext(`(${literal})`, Object.create(null), { timeout: 1000 }),
  };
}

function buildSorterLookup(orderData) {
  const full = new Map();
  const short = new Map();
  for (const [categoryKey, properties] of Object.entries(safeObject(orderData?.propertyRankByCategory))) {
    for (const [propertyKey, rank] of Object.entries(safeObject(properties))) {
      const normalizedPropertyKey = normalizeKey(propertyKey);
      const entry = { categoryKey, rank: Number(rank) };
      if (normalizedPropertyKey.includes(".")) {
        full.set(normalizedPropertyKey, entry);
      } else {
        const existing = short.get(normalizedPropertyKey) ?? [];
        existing.push(entry);
        short.set(normalizedPropertyKey, existing);
      }
    }
  }
  return { full, short };
}

function sorterMetadataForProperty(lookup, classChain, className, declaringClass, propertyName) {
  const classPriority = [className, declaringClass, ...[...classChain].reverse()]
    .filter(Boolean)
    .filter((value, index, values) => values.indexOf(value) === index);
  for (const candidateClass of classPriority) {
    const entry = lookup.full.get(normalizeKey(`${candidateClass}.${propertyName}`));
    if (entry) {
      return {
        category: categoryLabel(entry.categoryKey),
        order: Number.isFinite(entry.rank) ? entry.rank : undefined,
      };
    }
  }

  const shortEntries = lookup.short.get(normalizeKey(propertyName)) ?? [];
  const categoryKeys = new Set(shortEntries.map((entry) => entry.categoryKey));
  if (categoryKeys.size === 1) {
    const entry = shortEntries.reduce((best, candidate) => candidate.rank < best.rank ? candidate : best, shortEntries[0]);
    return {
      category: categoryLabel(entry.categoryKey),
      order: Number.isFinite(entry.rank) ? entry.rank : undefined,
    };
  }
  return {};
}

function fallbackCategory(className, name, property) {
  const dataType = propertyDataType(property) ?? "";
  const lower = name.toLowerCase();
  if (className === "Lighting") {
    if (
      dataType === "Color3" ||
      lower.includes("ambient") ||
      lower.includes("brightness") ||
      lower.includes("color") ||
      lower.includes("diffuse") ||
      lower.includes("specular") ||
      lower.includes("exposure") ||
      lower.includes("fog") ||
      lower.includes("shadow") ||
      lower.includes("time") ||
      lower.includes("technology") ||
      lower.includes("lightingstyle")
    ) {
      return "Appearance";
    }
  }
  if (
    lower.includes("enabled") ||
    lower.includes("disabled") ||
    lower === "runcontext" ||
    lower.includes("autoload") ||
    lower.includes("can") ||
    lower.includes("locked") ||
    lower.includes("visible") ||
    lower.includes("active") ||
    lower.includes("selectable") ||
    lower.includes("shadows") ||
    lower.includes("quality") ||
    lower.includes("respawn")
  ) {
    return "Behavior";
  }
  if (
    lower.includes("position") ||
    lower.includes("size") ||
    lower.includes("cframe") ||
    lower.includes("orientation") ||
    lower.includes("rotation") ||
    lower.includes("pivot") ||
    lower.includes("origin") ||
    lower.includes("scale") ||
    lower.includes("offset")
  ) {
    return "Transform";
  }
  if (lower.includes("text") || lower.includes("font") || lower.includes("lineheight")) {
    return "Text";
  }
  if (lower.includes("image") || lower.includes("slice") || lower.includes("tile")) {
    return "Image";
  }
  if (lower.includes("layout") || lower.includes("padding") || lower.includes("alignment") || lower.includes("sortorder")) {
    return "Layout";
  }
  if (lower.includes("localization") || lower.includes("localize")) {
    return "Localization";
  }
  return "Data";
}

function studioCategoryHint(className, propertyName, dataType, category) {
  if (className !== "Workspace") {
    return undefined;
  }
  if (propertyName === "SandboxedInstanceMode") {
    return "Permissions";
  }
  if (WORKSPACE_SERVER_AUTHORITY_PROPERTIES.has(propertyName)) {
    return "Server Authority";
  }
  const normalizedCategory = normalizeKey(category);
  if (normalizedCategory && normalizedCategory !== "behavior" && normalizedCategory !== "data") {
    return undefined;
  }
  const lower = propertyName.toLowerCase();
  const typeLower = String(dataType ?? "").toLowerCase();
  const text = `${lower} ${typeLower}`;
  if (text.includes("stream")) {
    return "Streaming";
  }
  if (text.includes("luau") || text.includes("typecheck")) {
    return "Scripting";
  }
  if (text.includes("authority")) {
    return "Server Authority";
  }
  if (text.includes("network") || text.includes("replicate") || text.includes("deletion")) {
    return "Networking";
  }
  if (
    text.includes("physics") ||
    text.includes("collision") ||
    text.includes("constraint") ||
    text.includes("fluid") ||
    text.includes("mover")
  ) {
    return "Physics";
  }
  if (
    text.includes("avatar") ||
    text.includes("character") ||
    text.includes("animator") ||
    text.includes("retarget") ||
    text.includes("accessor") ||
    text.includes("headsandaccessories") ||
    text.includes("ikcontrol")
  ) {
    return "Avatar";
  }
  if (text.includes("render")) {
    return "Rendering";
  }
  if (text.includes("pathfinding")) {
    return "Pathfinding";
  }
  if (text.includes("sandbox")) {
    return "Permissions";
  }
  return undefined;
}

function displaySourcePath(sourcePath, repoRoot) {
  if (!sourcePath) {
    return undefined;
  }
  const relativeToRepo = path.relative(repoRoot, sourcePath);
  if (relativeToRepo && !relativeToRepo.startsWith("..") && !path.isAbsolute(relativeToRepo)) {
    return relativeToRepo.replace(/\\/g, "/");
  }
  const localAppData = process.env.LOCALAPPDATA;
  if (localAppData) {
    const relativeToLocalAppData = path.relative(localAppData, sourcePath);
    if (relativeToLocalAppData && !relativeToLocalAppData.startsWith("..") && !path.isAbsolute(relativeToLocalAppData)) {
      return `%LOCALAPPDATA%/${relativeToLocalAppData.replace(/\\/g, "/")}`;
    }
  }
  return sourcePath;
}

function luaString(value) {
  return JSON.stringify(String(value));
}

function classPickerClassNames(classes) {
  return Object.keys(classes)
    .filter((className) => !classHasTag(classes, className, "NotCreatable"))
    .sort((a, b) => a.localeCompare(b));
}

function writeRobloxClassListModule(extensionRoot, classes) {
  const outputPath = path.join(extensionRoot, "src", GENERATED_CLASS_LIST_FILE_NAME);
  const classNames = classPickerClassNames(classes);
  const lines = [
    "// Generated by tools/renium-vscode-extension/scripts/generate-properties-metadata.mjs.",
    "// Uses the current Studio API dump as the class picker source; do not edit by hand.",
    "export const ROBLOX_CLASS_NAMES = [",
    ...classNames.map((className) => `  ${JSON.stringify(className)},`),
    "];",
    "",
  ];
  fs.writeFileSync(outputPath, lines.join("\n"), "utf8");
  return { outputPath, classCount: classNames.length };
}

function writeStudioApiSchemaModule(repoRoot, generatedClasses) {
  const pluginRoot = path.join(repoRoot, "tools", "plugin_ws_bridge");
  if (!fs.existsSync(pluginRoot)) {
    return undefined;
  }
  const outputPath = path.join(pluginRoot, GENERATED_STUDIO_API_SCHEMA_FILE_NAME);
  const lines = [
    "-- Generated by tools/renium-vscode-extension/scripts/generate-properties-metadata.mjs.",
    "-- Uses the current Studio API dump as the schema source; do not edit by hand.",
    "return {",
  ];
  for (const className of Object.keys(generatedClasses).sort((a, b) => a.localeCompare(b))) {
    const properties = generatedClasses[className];
    const entries = Object.entries(properties)
      .filter(([, info]) => info.writable !== false && typeof info.type === "string" && info.type !== "unknown")
      .sort(([aName, aInfo], [bName, bInfo]) => {
        const categorySort = String(aInfo.category).localeCompare(String(bInfo.category));
        return categorySort || Number(aInfo.order ?? 0) - Number(bInfo.order ?? 0) || aName.localeCompare(bName);
      });
    if (entries.length === 0) {
      continue;
    }
    lines.push(`\t[${luaString(className)}] = {`);
    for (const [propertyName, info] of entries) {
      lines.push(`\t\t{ ${luaString(propertyName)}, ${luaString(info.type)} },`);
    }
    lines.push("\t},");
  }
  lines.push("}");
  fs.writeFileSync(outputPath, `${lines.join("\n")}\n`, "utf8");
  return outputPath;
}

export function generateRobloxPropertiesMetadata(options = {}) {
  const extensionRoot = options.extensionRoot ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const repoRoot = options.repoRoot ?? path.resolve(extensionRoot, "..", "..");
  const outputPath = options.outputPath ?? path.join(extensionRoot, "resources", GENERATED_FILE_NAME);
  const propertySourcePath = resolvePropertySource(repoRoot);
  if (!propertySourcePath) {
    throw new Error("Could not find API-Dump.json, Full-API-Dump.json, or plugin rbx_dom database.json.");
  }

  const { classes, enums, sourceKind } = normalizePropertySource(JSON.parse(fs.readFileSync(propertySourcePath, "utf8")));
  const reflectionPath = findLatestReflectionMetadata(repoRoot);
  const reflection = reflectionPath
    ? parseReflectionMetadata(fs.readFileSync(reflectionPath, "utf8"))
    : { classes: {} };
  const sorter = loadSorterData(extensionRoot);
  const sorterLookup = buildSorterLookup(sorter.data);

  const generatedClasses = {};
  for (const className of Object.keys(classes).sort((a, b) => a.localeCompare(b))) {
    const chain = collectClassChain(classes, className);
    const generatedProperties = {};
    for (const declaringClass of chain) {
      const classInfo = classes[declaringClass];
      for (const [propertyName, property] of Object.entries(safeObject(classInfo?.Properties))) {
        if (!isWritablePropertyForClass(classes, className, declaringClass, propertyName, property)) {
          continue;
        }
        const reflectionInfo = reflection.classes?.[declaringClass]?.[propertyName] ?? reflection.classes?.[className]?.[propertyName];
        const sorterInfo = sorterMetadataForProperty(sorterLookup, chain, className, declaringClass, propertyName);
        const visible = reflectionInfo?.visible !== false;
        if (!visible) {
          continue;
        }
        const dataType = propertyDataType(property) ?? "unknown";
        const rawCategory = categoryOverride(className, propertyName) ??
          reflectionInfo?.category ??
          property.Category ??
          sorterInfo.category ??
          fallbackCategory(className, propertyName, property);
        const category = categoryLabel(studioCategoryHint(className, propertyName, dataType, rawCategory) ?? rawCategory);
        if (!shouldEmitProperty(className, propertyName, category)) {
          continue;
        }
        generatedProperties[propertyName] = {
          type: dataType,
          category,
          displayName: displayNameOverride(className, propertyName) ?? reflectionInfo?.displayName ?? propertyName,
          order: reflectionInfo?.order ?? sorterInfo.order ?? 0,
          writable: true,
          visible: true,
          declaringClass,
          enumItems: enumItemsForDataType(enums, dataType),
          uiMinimum: reflectionInfo?.uiMinimum,
          uiMaximum: reflectionInfo?.uiMaximum,
          uiNumTicks: reflectionInfo?.uiNumTicks,
          sliderScaling: reflectionInfo?.sliderScaling,
        };
      }
    }
    if (className === "Workspace") {
      for (const [propertyName, propertyInfo] of Object.entries(CURRENT_WORKSPACE_API_PROPERTIES)) {
        if (generatedProperties[propertyName]) {
          continue;
        }
        const sorterInfo = sorterMetadataForProperty(sorterLookup, chain, className, className, propertyName);
        generatedProperties[propertyName] = {
          type: propertyInfo.type,
          category: propertyInfo.category,
          displayName: propertyInfo.displayName ?? propertyName,
          order: sorterInfo.order ?? propertyInfo.order,
          writable: propertyInfo.writable !== false,
          visible: true,
          declaringClass: "Workspace",
          enumItems: enumItemsForDataType(enums, propertyInfo.type),
        };
      }
    }
    for (const [propertyName, propertyInfo] of Object.entries(safeObject(FORCED_STUDIO_API_PROPERTIES[className]))) {
      if (generatedProperties[propertyName]) {
        continue;
      }
      const sorterInfo = sorterMetadataForProperty(sorterLookup, chain, className, className, propertyName);
      generatedProperties[propertyName] = {
        type: propertyInfo.type,
        category: categoryLabel(propertyInfo.category ?? fallbackCategory(className, propertyName, {})),
        displayName: propertyInfo.displayName ?? propertyName,
        order: sorterInfo.order ?? propertyInfo.order ?? 0,
        writable: propertyInfo.writable !== false,
        visible: true,
        declaringClass: propertyInfo.declaringClass ?? className,
        enumItems: enumItemsForDataType(enums, propertyInfo.type),
      };
    }
    if (Object.keys(generatedProperties).length > 0) {
      generatedClasses[className] = Object.fromEntries(
        Object.entries(generatedProperties).sort(([aName, aValue], [bName, bValue]) => {
          const categorySort = String(aValue.category).localeCompare(String(bValue.category));
          return categorySort || Number(aValue.order ?? 0) - Number(bValue.order ?? 0) || aName.localeCompare(bName);
        }),
      );
    }
  }

  const payload = {
    version: 1,
    sources: {
      propertySource: displaySourcePath(propertySourcePath, repoRoot),
      propertySourceKind: sourceKind,
      reflectionMetadata: displaySourcePath(reflectionPath, repoRoot),
      propertySorter: displaySourcePath(sorter.path, repoRoot),
    },
    classes: generatedClasses,
  };
  fs.mkdirSync(path.dirname(outputPath), { recursive: true });
  fs.writeFileSync(outputPath, `${JSON.stringify(payload, null, 2)}\n`, "utf8");
  const studioApiSchemaPath = writeStudioApiSchemaModule(repoRoot, generatedClasses);
  const classList = writeRobloxClassListModule(extensionRoot, classes);
  return {
    outputPath,
    studioApiSchemaPath,
    classListPath: classList.outputPath,
    classCount: Object.keys(generatedClasses).length,
    classPickerCount: classList.classCount,
  };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const result = generateRobloxPropertiesMetadata();
  console.log(`Generated ${path.relative(process.cwd(), result.outputPath)} (${result.classCount} metadata classes)`);
  if (result.classListPath) {
    console.log(`Generated ${path.relative(process.cwd(), result.classListPath)} (${result.classPickerCount} picker classes)`);
  }
}
