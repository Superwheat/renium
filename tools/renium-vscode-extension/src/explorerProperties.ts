import * as fs from "fs";
import * as path from "path";

import type { ExplorerConfig, FileExplorerNode } from "./fileExplorerCore";
import { recordValue, safeArray, safeObject } from "./utils";

const PROTECTED_STARTER_PLAYER_CONTAINERS = new Set(["StarterCharacterScripts", "StarterPlayerScripts"]);
const MODEL_PIVOT_CLASSES = new Set(["Model", "WorldModel", "Workspace"]);
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
type RbxDomProperty = {
  Name?: string;
  MemberType?: string;
  Scriptability?: string;
  Security?: {
    Read?: string;
    Write?: string;
  };
  DataType?: {
    Value?: string;
    Enum?: string;
  };
  ValueType?: {
    Name?: string;
    Category?: string;
  };
  Category?: string;
  Tags?: string[];
  Kind?: unknown;
};

export function isModelPivotClass(className: string): boolean {
  return MODEL_PIVOT_CLASSES.has(className);
}

type RbxDomClass = {
  Name?: string;
  Superclass?: string;
  Tags?: string[];
  Members?: RbxDomProperty[];
  Properties?: Record<string, RbxDomProperty>;
  DefaultProperties?: Record<string, unknown>;
};

type RbxDomDatabase = {
  Classes?: Record<string, RbxDomClass>;
  Enums?: Record<string, { items?: Record<string, number> }>;
};

type GeneratedRobloxPropertyInfo = {
  type?: string;
  category?: string;
  displayName?: string;
  order?: number;
  writable?: boolean;
  visible?: boolean;
  declaringClass?: string;
  enumItems?: string[];
  uiMinimum?: number;
  uiMaximum?: number;
  uiNumTicks?: number;
  sliderScaling?: string;
};

type GeneratedRobloxProperties = {
  version?: number;
  classes?: Record<string, Record<string, GeneratedRobloxPropertyInfo>>;
};

type PropertyRow = {
  name: string;
  displayName?: string;
  value: unknown;
  readonly: boolean;
  defaulted: boolean;
  category: string;
  order: number;
  dataType?: string;
  enumItems?: string[];
  uiMinimum?: number;
  uiMaximum?: number;
  uiNumTicks?: number;
  sliderScaling?: string;
};

type PropertyTemplate = Omit<PropertyRow, "value" | "defaulted"> & {
  defaultValue: unknown;
};

type VerdePropertyInfo = {
  name: string;
  displayName?: string;
  type: string;
  value: unknown;
  category: string;
  layoutOrder?: number;
  isEnum?: boolean;
  enumValues?: Array<{ name: string; value: number }>;
  displayValue?: string;
  isInstanceReference?: boolean;
  referencedInstanceId?: string;
  referencedInstanceName?: string;
  referencedInstanceClass?: string;
  isReadOnly?: boolean;
  uiMinimum?: number;
  uiMaximum?: number;
  uiNumTicks?: number;
  sliderScaling?: string;
};

type VerdeAttributeInfo = {
  name: string;
  type: string;
  value: unknown;
};

export type VerdePropertiesData = {
  properties: VerdePropertyInfo[];
  tags: string[];
  attributes: VerdeAttributeInfo[];
};
let rbxDomDatabaseCache: RbxDomDatabase | undefined;
let generatedRobloxPropertiesCache: GeneratedRobloxProperties | undefined;
const propertyTemplateCache = new Map<string, PropertyTemplate[]>();
const scriptDisabledClasses = new Set(["Script", "LocalScript"]);
const valueInstanceFallbackTypes: Record<string, string> = {
  BinaryStringValue: "BinaryString",
};
export function isProtectedStarterPlayerContainer(node: FileExplorerNode): boolean {
  return node.kind === "instance" &&
    node.service === "StarterPlayer" &&
    node.parentTreeId === "service:StarterPlayer" &&
    PROTECTED_STARTER_PLAYER_CONTAINERS.has(node.name);
}

function loadRbxDomDatabase(config: ExplorerConfig): RbxDomDatabase | undefined {
  if (rbxDomDatabaseCache !== undefined) {
    return rbxDomDatabaseCache;
  }
  const databasePath = [
    path.join(config.projectRoot, "API-Dump.json"),
    path.join(config.projectRoot, "Full-API-Dump.json"),
    path.join(config.projectRoot, "tools", "plugin_ws_bridge", "rbx_dom_lua", "database.json"),
  ].find((candidate) => fs.existsSync(candidate));
  if (!databasePath) {
    rbxDomDatabaseCache = {};
    return rbxDomDatabaseCache;
  }
  try {
    rbxDomDatabaseCache = normalizeRbxDomDatabase(JSON.parse(fs.readFileSync(databasePath, "utf8")));
  } catch {
    rbxDomDatabaseCache = {};
  }
  return rbxDomDatabaseCache;
}

function loadGeneratedRobloxProperties(config: ExplorerConfig): GeneratedRobloxProperties | undefined {
  if (generatedRobloxPropertiesCache !== undefined) {
    return generatedRobloxPropertiesCache;
  }
  const extensionRoot = path.resolve(__dirname, "..");
  const metadataPath = [
    path.join(extensionRoot, "resources", "roblox-properties.generated.json"),
    path.join(config.projectRoot, "tools", "renium-vscode-extension", "resources", "roblox-properties.generated.json"),
  ].find((candidate) => fs.existsSync(candidate));
  if (!metadataPath) {
    generatedRobloxPropertiesCache = {};
    return generatedRobloxPropertiesCache;
  }
  try {
    generatedRobloxPropertiesCache = JSON.parse(fs.readFileSync(metadataPath, "utf8")) as GeneratedRobloxProperties;
  } catch {
    generatedRobloxPropertiesCache = {};
  }
  return generatedRobloxPropertiesCache;
}

function generatedPropertyInfo(
  metadata: GeneratedRobloxProperties | undefined,
  className: string,
  propertyName: string,
): GeneratedRobloxPropertyInfo | undefined {
  const byClass = metadata?.classes?.[className];
  return byClass?.[propertyName];
}

function hasGeneratedPropertyList(metadata: GeneratedRobloxProperties | undefined, className: string): boolean {
  return !!metadata?.classes?.[className];
}

function isGeneratedPropertyVisible(
  metadata: GeneratedRobloxProperties | undefined,
  className: string,
  propertyName: string,
): boolean {
  const byClass = metadata?.classes?.[className];
  return !byClass || !!byClass[propertyName];
}

function normalizeRbxDomDatabase(raw: unknown): RbxDomDatabase {
  const record = safeObject(raw);
  if (!Array.isArray(record.Classes)) {
    return record as RbxDomDatabase;
  }

  const classes: Record<string, RbxDomClass> = {};
  for (const rawClass of record.Classes) {
    const classRecord = safeObject(rawClass);
    const className = String(classRecord.Name ?? "");
    if (!className) {
      continue;
    }
    const properties: Record<string, RbxDomProperty> = {};
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
        Security: safeObject(member.Security) as RbxDomProperty["Security"],
        ValueType: { Name: valueTypeName, Category: valueTypeCategory },
        DataType: rbxDomDataTypeFromApiDumpValueType(valueTypeName, valueTypeCategory),
        Category: typeof member.Category === "string" ? member.Category : undefined,
        Tags: safeArray(member.Tags).map((tag) => String(tag)),
      };
    }
    classes[className] = {
      Name: className,
      Superclass: typeof classRecord.Superclass === "string" ? classRecord.Superclass : undefined,
      Tags: safeArray(classRecord.Tags).map((tag) => String(tag)),
      Properties: properties,
      DefaultProperties: {},
    };
  }

  const enums: Record<string, { items?: Record<string, number> }> = {};
  for (const rawEnum of safeArray(record.Enums)) {
    const enumRecord = safeObject(rawEnum);
    const enumName = String(enumRecord.Name ?? "");
    if (!enumName) {
      continue;
    }
    const items: Record<string, number> = {};
    for (const rawItem of safeArray(enumRecord.Items)) {
      const item = safeObject(rawItem);
      const itemName = String(item.Name ?? "");
      const itemValue = typeof item.Value === "number" ? item.Value : Number(item.Value);
      if (itemName && Number.isFinite(itemValue)) {
        items[itemName] = itemValue;
      }
    }
    enums[enumName] = { items };
  }

  return { Classes: classes, Enums: enums };
}

function rbxDomDataTypeFromApiDumpValueType(name: string | undefined, category: string | undefined): RbxDomProperty["DataType"] {
  if (!name) {
    return undefined;
  }
  if (category === "Enum") {
    return { Enum: name.replace(/^Enum\./, "") };
  }
  if (category === "Class") {
    return { Value: "Ref" };
  }
  const primitiveMap: Record<string, string> = {
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

function findRbxDomProperty(classes: Record<string, RbxDomClass>, className: string, propertyName: string): RbxDomProperty | undefined {
  const seen = new Set<string>();
  let current: string | undefined = className;
  while (current && !seen.has(current)) {
    seen.add(current);
    const classInfo: RbxDomClass | undefined = classes[current];
    const property = classInfo?.Properties?.[propertyName];
    if (property) {
      return property;
    }
    current = classInfo?.Superclass;
  }
  return fallbackValueInstanceProperty(className, propertyName);
}

function fallbackValueInstanceProperty(className: string, propertyName: string): RbxDomProperty | undefined {
  const valueType = valueInstanceFallbackTypes[className];
  if (!valueType || propertyName !== "Value") {
    return undefined;
  }
  return {
    Name: "Value",
    Scriptability: "ReadWrite",
    DataType: { Value: valueType },
    Tags: [],
  };
}

function collectRbxDomClassChain(classes: Record<string, RbxDomClass>, className: string): string[] {
  const chain: string[] = [];
  const seen = new Set<string>();
  let current: string | undefined = className;
  while (current && !seen.has(current)) {
    seen.add(current);
    const classInfo: RbxDomClass | undefined = classes[current];
    if (!classInfo) {
      break;
    }
    chain.unshift(current);
    current = classInfo.Superclass;
  }
  return chain;
}

function propertyTags(property: RbxDomProperty | undefined): Set<string> {
  return new Set(Array.isArray(property?.Tags) ? property.Tags : []);
}

function classHasDefaultProperty(classes: Record<string, RbxDomClass> | undefined, className: string, propertyName: string): boolean {
  const defaults = classes?.[className]?.DefaultProperties;
  return !!defaults && Object.prototype.hasOwnProperty.call(defaults, propertyName);
}

function isDefaultBackedStudioProperty(
  classes: Record<string, RbxDomClass> | undefined,
  className: string,
  declaringClassName: string | undefined,
  propertyName: string,
): boolean {
  return classHasDefaultProperty(classes, className, propertyName) ||
    (!!declaringClassName && classHasDefaultProperty(classes, declaringClassName, propertyName));
}

function allowsHiddenDefaultBackedStudioProperty(name: string, property: RbxDomProperty | undefined): boolean {
  if (name === "AvatarJointUpgrade_SerializedRollout") {
    return true;
  }
  const tags = propertyTags(property);
  if (!tags.has("Hidden")) {
    return false;
  }
  if (!tags.has("NotScriptable") || tags.has("NotReplicated")) {
    return false;
  }
  if (propertyDataType(property) !== "Enum.LoadCharacterLayeredClothing") {
    return false;
  }
  return !/^GameSettings/i.test(name) && !/Serialized|Rollout/i.test(name);
}

function hasBlockedStudioPropertyTag(property: RbxDomProperty | undefined, allowHiddenStudioProperty = false): boolean {
  const tags = propertyTags(property);
  return tags.has("ReadOnly") ||
    (tags.has("Hidden") && !allowHiddenStudioProperty) ||
    tags.has("Deprecated") ||
    tags.has("NotBrowsable") ||
    tags.has("WriteOnly");
}

function isSerializedStudioProperty(property: RbxDomProperty | undefined): boolean {
  const kind = property?.Kind;
  if (!kind || typeof kind !== "object" || Array.isArray(kind)) {
    return true;
  }
  const kindRecord = kind as Record<string, unknown>;
  if (kindRecord.Alias && typeof kindRecord.Alias === "object") {
    return false;
  }
  const canonical = kindRecord.Canonical;
  if (!canonical || typeof canonical !== "object" || Array.isArray(canonical)) {
    return true;
  }
  const serialization = (canonical as Record<string, unknown>).Serialization;
  return serialization !== "DoesNotSerialize";
}

function propertyCanonicalSerialization(property: RbxDomProperty | undefined): unknown {
  const kind = property?.Kind;
  if (!kind || typeof kind !== "object" || Array.isArray(kind)) {
    return undefined;
  }
  const canonical = (kind as Record<string, unknown>).Canonical;
  if (!canonical || typeof canonical !== "object" || Array.isArray(canonical)) {
    return undefined;
  }
  return (canonical as Record<string, unknown>).Serialization;
}

function propertyMigrationTarget(property: RbxDomProperty | undefined): string | undefined {
  const serialization = propertyCanonicalSerialization(property);
  if (!serialization || typeof serialization !== "object" || Array.isArray(serialization)) {
    return undefined;
  }
  const migrate = (serialization as Record<string, unknown>).Migrate;
  if (!migrate || typeof migrate !== "object" || Array.isArray(migrate)) {
    return undefined;
  }
  const to = (migrate as Record<string, unknown>).To;
  return typeof to === "string" ? to : undefined;
}

function isSupersededMigratedProperty(
  className: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
): boolean {
  const target = propertyMigrationTarget(property);
  if (!target || !classes) {
    return false;
  }
  const targetProperty = findRbxDomProperty(classes, className, target);
  if (!targetProperty) {
    return false;
  }
  if (propertyCanonicalSerialization(targetProperty) === "DoesNotSerialize") {
    return true;
  }
  return isWritableStudioProperty(targetProperty);
}

function isWritableStudioProperty(
  property: RbxDomProperty | undefined,
  allowHiddenStudioProperty = false,
  allowNonSerializedStudioProperty = false,
): boolean {
  if (!property) {
    return false;
  }
  if (property.MemberType && property.MemberType !== "Property") {
    return false;
  }
  if (hasBlockedStudioPropertyTag(property, allowHiddenStudioProperty)) {
    return false;
  }
  return allowNonSerializedStudioProperty || isSerializedStudioProperty(property);
}

function classHasTag(classes: Record<string, RbxDomClass> | undefined, className: string, tag: string): boolean {
  const tags = classes?.[className]?.Tags;
  return Array.isArray(tags) && tags.includes(tag);
}

function allowsNonSerializedStudioProperty(className: string, name: string): boolean {
  return (name === "WorldPivot" && MODEL_PIVOT_CLASSES.has(className)) ||
    (className === "Workspace" && WORKSPACE_VISIBLE_NON_SERIALIZED_PROPERTIES.has(name));
}

function allowsServiceRefStudioProperty(className: string, name: string): boolean {
  return className === "Workspace" && WORKSPACE_VISIBLE_SERVICE_REF_PROPERTIES.has(name);
}

function isEngineManagedStudioProperty(
  className: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  propertyName?: string,
): boolean {
  const dataType = propertyDataType(property);
  return dataType === "UniqueId" ||
    dataType === "SecurityCapabilities" ||
    (dataType === "Ref" && classHasTag(classes, className, "Service") && !allowsServiceRefStudioProperty(className, propertyName ?? ""));
}

function isVisibleStudioProperty(
  className: string,
  name: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  declaringClassName?: string,
): boolean {
  if (!property) {
    return false;
  }
  if (
    name === "Name" ||
    name === "ClassName" ||
    name === "Parent" ||
    name === "Sandboxed" ||
    name === "DefinesCapabilities" ||
    name === "Attributes" ||
    name === "Tags" ||
    name === "Source" ||
    name === "LinkedSource"
  ) {
    return false;
  }
  if (className === "Workspace" && WORKSPACE_HIDDEN_STUDIO_PROPERTIES.has(name)) {
    return false;
  }
  if (isEngineManagedStudioProperty(className, property, classes, name)) {
    return false;
  }
  if (isSupersededMigratedProperty(className, property, classes)) {
    return false;
  }
  const allowHidden = isDefaultBackedStudioProperty(classes, className, declaringClassName, name) &&
    allowsHiddenDefaultBackedStudioProperty(name, property);
  return isWritableStudioProperty(property, allowHidden, allowsNonSerializedStudioProperty(className, name));
}

export function isMetadataPropertyName(name: string): boolean {
  return name.toLowerCase() === "name" || name.toLowerCase() === "classname" || name.toLowerCase() === "parent";
}

function isVisibleStudioPropertyForNode(
  node: FileExplorerNode,
  name: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  declaringClassName?: string,
): boolean {
  if (isMetadataPropertyName(name)) {
    return false;
  }
  return isVisibleStudioProperty(node.className, name, property, classes, declaringClassName);
}

function isReadonlyStudioProperty(
  property: RbxDomProperty | undefined,
  allowDefaultBackedStudioProperty = false,
  allowNonSerializedStudioProperty = false,
): boolean {
  return !isWritableStudioProperty(property, allowDefaultBackedStudioProperty, allowNonSerializedStudioProperty);
}

function isReadonlyStudioPropertyForNode(
  node: FileExplorerNode,
  name: string,
  property: RbxDomProperty | undefined,
  classes?: Record<string, RbxDomClass>,
  declaringClassName?: string,
): boolean {
  const allowHidden = isDefaultBackedStudioProperty(classes, node.className, declaringClassName, name) &&
    allowsHiddenDefaultBackedStudioProperty(name, property);
  return isReadonlyStudioProperty(property, allowHidden, allowsNonSerializedStudioProperty(node.className, name));
}

export function usesDisabledProperty(className: string): boolean {
  return scriptDisabledClasses.has(className);
}

function propertyDataType(property: RbxDomProperty | undefined): string | undefined {
  return property?.DataType?.Enum ? `Enum.${property.DataType.Enum}` : property?.DataType?.Value;
}

function propertyDisplayName(metadata: GeneratedRobloxProperties | undefined, className: string, name: string): string | undefined {
  const generated = generatedPropertyInfo(metadata, className, name);
  return generated?.displayName && generated.displayName !== name ? generated.displayName : undefined;
}

function propertyCategory(
  className: string,
  name: string,
  property: RbxDomProperty | undefined,
  metadata?: GeneratedRobloxProperties,
): string {
  const generated = generatedPropertyInfo(metadata, className, name);
  if (generated?.category) {
    return generated.category;
  }
  if (name === "Archivable") {
    return "Data";
  }
  if (className === "Workspace" && name === "SandboxedInstanceMode") {
    return "Permissions";
  }
  if (className === "Workspace" && WORKSPACE_SERVER_AUTHORITY_PROPERTIES.has(name)) {
    return "Server Authority";
  }
  if (property?.Category) {
    return property.Category;
  }
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

function propertyOrder(
  metadata: GeneratedRobloxProperties | undefined,
  className: string,
  name: string,
  fallbackOrder: number,
): number {
  const generatedOrder = generatedPropertyInfo(metadata, className, name)?.order;
  return typeof generatedOrder === "number" && Number.isFinite(generatedOrder) ? generatedOrder : fallbackOrder;
}

function propertyCategoryRank(category: string): number {
  const order = [
    "Data",
    "Camera",
    "Character",
    "Character Jump Settings",
    "Controls",
    "Mobile",
    "Permissions",
    "Behavior",
    "Appearance",
    "Pivot",
    "Transform",
    "Air Properties",
    "Avatar",
    "Networking",
    "Physics",
    "Pathfinding",
    "Rendering",
    "Scripting",
    "Server Authority",
    "Streaming",
    "Text",
    "Image",
    "Layout",
    "Localization",
    "Tags",
    "Attributes",
  ];
  const index = order.indexOf(category);
  return index === -1 ? order.length : index;
}

function comparePropertyRows<T extends { category: string; order: number; name: string }>(a: T, b: T): number {
  const categorySort = propertyCategoryRank(a.category) - propertyCategoryRank(b.category);
  return categorySort || a.category.localeCompare(b.category) || a.order - b.order || a.name.localeCompare(b.name);
}

function sortPropertyRows<T extends { category: string; order: number; name: string }>(rows: T[]): T[] {
  return rows.sort(comparePropertyRows);
}

function enumItemsForProperty(property: RbxDomProperty | undefined, database: RbxDomDatabase): string[] | undefined {
  const enumType = property?.DataType?.Enum;
  if (!enumType) {
    return undefined;
  }
  const items = database.Enums?.[enumType]?.items;
  if (!items) {
    return undefined;
  }
  return Object.entries(items)
    .sort((a, b) => a[1] - b[1])
    .map(([name]) => name);
}

function propertyFromGeneratedInfo(info: GeneratedRobloxPropertyInfo | undefined): RbxDomProperty | undefined {
  const type = info?.type;
  if (!type) {
    return undefined;
  }
  if (type.startsWith("Enum.")) {
    return { MemberType: "Property", DataType: { Enum: type.slice("Enum.".length) } };
  }
  return { MemberType: "Property", DataType: { Value: type } };
}

function enumItemsForGeneratedInfo(info: GeneratedRobloxPropertyInfo | undefined, property: RbxDomProperty | undefined, database: RbxDomDatabase): string[] | undefined {
  return info?.enumItems ?? enumItemsForProperty(property, database);
}

function defaultValueForDataType(dataType: string | undefined, database: RbxDomDatabase, enumItems?: string[]): unknown {
  if (dataType?.startsWith("Enum.")) {
    const enumValues = enumValuesForDataType(dataType, database, enumItems);
    const enumName = enumValues?.find((item) => item.name === "Default")?.name ?? enumValues?.[0]?.name ?? "";
    return { _type: "EnumItem", enumType: dataType, name: enumName };
  }
  switch (dataType) {
    case "Bool":
      return false;
    case "Int32":
    case "Int64":
    case "Float32":
    case "Float64":
    case "Double":
      return 0;
    case "Vector2":
      return { _type: "Vector2", x: 0, y: 0 };
    case "Vector3":
      return { _type: "Vector3", x: 0, y: 0, z: 0 };
    case "CFrame":
    case "OptionalCFrame":
      return defaultCFrameValue();
    case "Color3":
      return { _type: "Color3", r: 0, g: 0, b: 0 };
    case "BrickColor":
      return { _type: "BrickColor", number: 194 };
    case "Ref":
      return null;
    default:
      return "";
  }
}

function unwrapDefaultPropertyValue(raw: unknown, property: RbxDomProperty | undefined, database: RbxDomDatabase): unknown {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    return raw;
  }
  const entries = Object.entries(raw as Record<string, unknown>);
  if (entries.length !== 1) {
    return raw;
  }
  const [kind, value] = entries[0];
  switch (kind) {
    case "Bool":
    case "Int32":
    case "Int64":
    case "Float32":
    case "Float64":
    case "OptionalCFrame":
    case "String":
    case "ContentId":
      return value;
    case "Enum": {
      const enumType = property?.DataType?.Enum;
      if (enumType && typeof value === "number") {
        const enumItems = database.Enums?.[enumType]?.items ?? {};
        const itemName = Object.entries(enumItems).find(([, enumValue]) => enumValue === value)?.[0];
        return {
          _type: "EnumItem",
          enumType: `Enum.${enumType}`,
          name: itemName ?? String(value),
        };
      }
      return value;
    }
    case "BrickColor":
      return { _type: "BrickColor", number: value };
    case "Color3":
      if (Array.isArray(value)) {
        return { _type: "Color3", r: value[0] ?? 0, g: value[1] ?? 0, b: value[2] ?? 0 };
      }
      return value;
    case "Color3uint8":
      if (Array.isArray(value)) {
        return {
          _type: "Color3",
          r: Number(value[0] ?? 0) / 255,
          g: Number(value[1] ?? 0) / 255,
          b: Number(value[2] ?? 0) / 255,
        };
      }
      return value;
    case "Vector2":
      if (Array.isArray(value)) {
        return { _type: "Vector2", x: value[0] ?? 0, y: value[1] ?? 0 };
      }
      return value;
    case "Vector3":
      if (Array.isArray(value)) {
        return { _type: "Vector3", x: value[0] ?? 0, y: value[1] ?? 0, z: value[2] ?? 0 };
      }
      return value;
    case "UDim":
      if (Array.isArray(value)) {
        return { _type: "UDim", scale: value[0] ?? 0, offset: value[1] ?? 0 };
      }
      return value;
    case "UDim2":
      if (Array.isArray(value) && Array.isArray(value[0]) && Array.isArray(value[1])) {
        return {
          _type: "UDim2",
          xScale: value[0][0] ?? 0,
          xOffset: value[0][1] ?? 0,
          yScale: value[1][0] ?? 0,
          yOffset: value[1][1] ?? 0,
        };
      }
      return value;
    case "CFrame":
      if (value && typeof value === "object" && !Array.isArray(value)) {
        const obj = value as { position?: unknown; orientation?: unknown };
        const position = Array.isArray(obj.position) ? obj.position : [0, 0, 0];
        const orientation = Array.isArray(obj.orientation) ? obj.orientation : [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
        const row0 = Array.isArray(orientation[0]) ? orientation[0] : [1, 0, 0];
        const row1 = Array.isArray(orientation[1]) ? orientation[1] : [0, 1, 0];
        const row2 = Array.isArray(orientation[2]) ? orientation[2] : [0, 0, 1];
        return {
          _type: "CFrame",
          components: [
            position[0] ?? 0,
            position[1] ?? 0,
            position[2] ?? 0,
            row0[0] ?? 1,
            row0[1] ?? 0,
            row0[2] ?? 0,
            row1[0] ?? 0,
            row1[1] ?? 1,
            row1[2] ?? 0,
            row2[0] ?? 0,
            row2[1] ?? 0,
            row2[2] ?? 1,
          ],
        };
      }
      return value;
    default:
      return raw;
  }
}

function propertyTemplatesForClass(
  className: string,
  database: RbxDomDatabase,
  classes: Record<string, RbxDomClass>,
  generatedMetadata?: GeneratedRobloxProperties,
): PropertyTemplate[] {
  const cached = propertyTemplateCache.get(className);
  if (cached) {
    return cached;
  }
  const rows = new Map<string, PropertyTemplate>();
  let nextOrder = 0;
  const pseudoNode = { className } as FileExplorerNode;
  const setTemplate = (name: string, property: RbxDomProperty | undefined, defaultValue: unknown, declaringClassName?: string): void => {
    if (hasGeneratedPropertyList(generatedMetadata, className) && !isGeneratedPropertyVisible(generatedMetadata, className, name)) {
      return;
    }
    const existing = rows.get(name);
    const generated = generatedPropertyInfo(generatedMetadata, className, name);
    const fallbackOrder = existing?.order ?? nextOrder++;
    rows.set(name, {
      name,
      displayName: existing?.displayName ?? propertyDisplayName(generatedMetadata, className, name),
      defaultValue,
      readonly: isReadonlyStudioPropertyForNode(pseudoNode, name, property, classes, declaringClassName),
      category: existing?.category ?? propertyCategory(className, name, property, generatedMetadata),
      order: propertyOrder(generatedMetadata, className, name, fallbackOrder),
      dataType: propertyDataType(property),
      enumItems: enumItemsForProperty(property, database),
      uiMinimum: existing?.uiMinimum ?? generated?.uiMinimum,
      uiMaximum: existing?.uiMaximum ?? generated?.uiMaximum,
      uiNumTicks: existing?.uiNumTicks ?? generated?.uiNumTicks,
      sliderScaling: existing?.sliderScaling ?? generated?.sliderScaling,
    });
  };
  const chain = collectRbxDomClassChain(classes, className);
  for (const chainClassName of chain) {
    const classInfo = classes[chainClassName];
    for (const [name, property] of Object.entries(classInfo?.Properties ?? {})) {
      if (hasGeneratedPropertyList(generatedMetadata, className) && !isGeneratedPropertyVisible(generatedMetadata, className, name)) {
        continue;
      }
      if (!isVisibleStudioPropertyForNode(pseudoNode, name, property, classes, chainClassName)) {
        continue;
      }
      const defaultRaw = classInfo?.DefaultProperties?.[name];
      setTemplate(name, property, defaultRaw === undefined ? "" : unwrapDefaultPropertyValue(defaultRaw, property, database), chainClassName);
    }
    for (const [name, defaultRaw] of Object.entries(classInfo?.DefaultProperties ?? {})) {
      if (hasGeneratedPropertyList(generatedMetadata, className) && !isGeneratedPropertyVisible(generatedMetadata, className, name)) {
        continue;
      }
      const property = findRbxDomProperty(classes, className, name);
      if (!isVisibleStudioPropertyForNode(pseudoNode, name, property, classes, chainClassName)) {
        continue;
      }
      setTemplate(name, property, unwrapDefaultPropertyValue(defaultRaw, property, database), chainClassName);
    }
  }
  const fallbackValueProperty = fallbackValueInstanceProperty(className, "Value");
  if (fallbackValueProperty && !rows.has("Value")) {
    setTemplate(
      "Value",
      fallbackValueProperty,
      defaultValueForDataType(propertyDataType(fallbackValueProperty), database),
      className,
    );
  }
  for (const [name, info] of Object.entries(generatedMetadata?.classes?.[className] ?? {})) {
    if (rows.has(name) || info.visible === false) {
      continue;
    }
    const dataType = info.type;
    const property = propertyFromGeneratedInfo(info);
    const enumItems = enumItemsForGeneratedInfo(info, property, database);
    rows.set(name, {
      name,
      displayName: info.displayName && info.displayName !== name ? info.displayName : undefined,
      defaultValue: defaultValueForDataType(dataType, database, enumItems),
      readonly: info.writable === false,
      category: info.category ?? propertyCategory(className, name, property, generatedMetadata),
      order: propertyOrder(generatedMetadata, className, name, nextOrder++),
      dataType,
      enumItems,
      uiMinimum: info.uiMinimum,
      uiMaximum: info.uiMaximum,
      uiNumTicks: info.uiNumTicks,
      sliderScaling: info.sliderScaling,
    });
  }
  const templates = sortPropertyRows(Array.from(rows.values()));
  propertyTemplateCache.set(className, templates);
  return templates;
}

export function propertyRowsForNode(node: FileExplorerNode, config: ExplorerConfig): PropertyRow[] {
  const database = loadRbxDomDatabase(config) ?? {};
  const generatedMetadata = loadGeneratedRobloxProperties(config);
  const classes = database.Classes ?? {};
  const rows = new Map<string, PropertyRow>();
  let nextOrder = 0;
  const setEnabledRow = (row: Omit<PropertyRow, "name">, value: boolean): void => {
    rows.set("Enabled", {
      name: "Enabled",
      displayName: "Enabled",
      value,
      readonly: row.readonly,
      defaulted: row.defaulted,
      category: row.category,
      order: rows.get("Enabled")?.order ?? row.order,
      dataType: "Bool",
      enumItems: row.enumItems,
    });
  };
  const setRow = (name: string, row: Omit<PropertyRow, "name">): void => {
    if (usesDisabledProperty(node.className) && name === "Disabled") {
      const disabledValue = row.value === true || String(row.value).toLowerCase() === "true";
      setEnabledRow(row, !disabledValue);
      return;
    }
    if (usesDisabledProperty(node.className) && name === "Enabled") {
      const existingEnabled = rows.get("Enabled");
      if (existingEnabled && !existingEnabled.defaulted) {
        return;
      }
      if (existingEnabled && row.defaulted) {
        return;
      }
      setEnabledRow(row, row.value === true || String(row.value).toLowerCase() === "true");
      return;
    }
    rows.set(name, { name, ...row });
  };
  const finalizeRows = (): PropertyRow[] => withStudioDuplicatePropertyRows(sortPropertyRows(Array.from(rows.values())), node);
  const templates = propertyTemplatesForClass(node.className, database, classes, generatedMetadata);
  for (const template of templates) {
    setRow(template.name, {
      displayName: template.displayName,
      value: template.defaultValue,
      readonly: template.readonly,
      defaulted: true,
      category: template.category,
      order: template.order,
      dataType: template.dataType,
      enumItems: template.enumItems,
      uiMinimum: template.uiMinimum,
      uiMaximum: template.uiMaximum,
      uiNumTicks: template.uiNumTicks,
      sliderScaling: template.sliderScaling,
    });
  }
  nextOrder = templates.length;
  for (const propertyName of Object.keys(node.properties)) {
    if (hasGeneratedPropertyList(generatedMetadata, node.className) && !isGeneratedPropertyVisible(generatedMetadata, node.className, propertyName)) {
      continue;
    }
    const property = findRbxDomProperty(classes, node.className, propertyName);
    if (property && isSupersededMigratedProperty(node.className, property, classes)) {
      const targetName = propertyMigrationTarget(property);
      if (targetName && !Object.prototype.hasOwnProperty.call(node.properties, targetName)) {
        const targetRow = rows.get(targetName);
        if (targetRow && targetRow.defaulted) {
          rows.set(targetName, { ...targetRow, value: node.properties[propertyName], defaulted: false });
        }
      }
      continue;
    }
    const generated = generatedPropertyInfo(generatedMetadata, node.className, propertyName);
    const generatedProperty = propertyFromGeneratedInfo(generated);
    if (isMetadataPropertyName(propertyName) || propertyName === "Tags" || propertyName === "Attributes" || isEngineManagedStudioProperty(node.className, property, classes, propertyName)) {
      continue;
    }
    const existing = rows.get(propertyName);
    if (property && !isVisibleStudioPropertyForNode(node, propertyName, property, classes) && !existing) {
      continue;
    }
    setRow(propertyName, {
      displayName: existing?.displayName ?? propertyDisplayName(generatedMetadata, node.className, propertyName),
      value: node.properties[propertyName],
      readonly: existing?.readonly ?? isReadonlyStudioPropertyForNode(node, propertyName, property, classes),
      defaulted: false,
      category: existing?.category ?? propertyCategory(node.className, propertyName, property, generatedMetadata),
      order: existing?.order ?? propertyOrder(generatedMetadata, node.className, propertyName, nextOrder++),
      dataType: existing?.dataType ?? generated?.type ?? propertyDataType(property),
      enumItems: existing?.enumItems ?? enumItemsForGeneratedInfo(generated, property ?? generatedProperty, database),
      uiMinimum: existing?.uiMinimum ?? generated?.uiMinimum,
      uiMaximum: existing?.uiMaximum ?? generated?.uiMaximum,
      uiNumTicks: existing?.uiNumTicks ?? generated?.uiNumTicks,
      sliderScaling: existing?.sliderScaling ?? generated?.sliderScaling,
    });
  }
  ensureModelPivotRows(node, rows, classes, nextOrder);
  return finalizeRows();
}

function withStudioDuplicatePropertyRows(rows: PropertyRow[], node: FileExplorerNode): PropertyRow[] {
  if (node.className !== "Workspace") {
    return rows;
  }
  const streamingEnabled = rows.find((row) => row.name === "StreamingEnabled");
  if (!streamingEnabled || rows.some((row) => row.name === "StreamingEnabled" && row.category === "Streaming")) {
    return rows;
  }
  return sortPropertyRows([
    ...rows,
    {
      ...streamingEnabled,
      category: "Streaming",
      order: 2,
    },
  ]);
}

function defaultCFrameValue(): unknown {
  return { _type: "CFrame", components: [0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1] };
}

export function modelPivotValue(node: FileExplorerNode): unknown {
  return node.properties.WorldPivot ?? node.properties.WorldPivotData ?? node.properties.Origin ?? defaultCFrameValue();
}

export function isModelPivotCFrameProperty(node: FileExplorerNode, name: string): boolean {
  return MODEL_PIVOT_CLASSES.has(node.className) && (name === "WorldPivot" || name === "WorldPivotData" || name === "Origin");
}

function ensureModelPivotRows(
  node: FileExplorerNode,
  rows: Map<string, PropertyRow>,
  classes: Record<string, RbxDomClass> | undefined,
  baseOrder: number,
): void {
  if (!MODEL_PIVOT_CLASSES.has(node.className)) {
    return;
  }
  rows.delete("WorldPivotData");
  rows.delete("Origin");
  const propertyFor = (name: string): RbxDomProperty | undefined => classes
    ? findRbxDomProperty(classes, node.className, name)
    : undefined;
  const put = (
    name: string,
    value: unknown,
    property: RbxDomProperty | undefined,
    dataType: string | undefined,
    orderOffset: number,
  ): void => {
    if (property && !isVisibleStudioPropertyForNode(node, name, property, classes)) {
      return;
    }
    const existing = rows.get(name);
    rows.set(name, {
      name,
      displayName: existing?.displayName,
      value: existing?.defaulted === false ? existing.value : value,
      readonly: existing?.readonly ?? isReadonlyStudioPropertyForNode(node, name, property, classes),
      defaulted: existing?.defaulted ?? !Object.prototype.hasOwnProperty.call(node.properties, name),
      category: existing?.category ?? "Pivot",
      order: existing?.order ?? baseOrder + orderOffset,
      dataType: existing?.dataType ?? dataType,
      enumItems: existing?.enumItems,
      uiMinimum: existing?.uiMinimum,
      uiMaximum: existing?.uiMaximum,
      uiNumTicks: existing?.uiNumTicks,
      sliderScaling: existing?.sliderScaling,
    });
  };

  const primaryPartProperty = propertyFor("PrimaryPart");
  put("PrimaryPart", node.properties.PrimaryPart ?? null, primaryPartProperty, propertyDataType(primaryPartProperty) ?? "Ref", 1);

  if (node.className !== "Workspace") {
    const scaleProperty = propertyFor("Scale");
    put("Scale", node.properties.Scale ?? 1, scaleProperty, propertyDataType(scaleProperty) ?? "Float32", 2);
  }

  const worldPivotProperty = propertyFor("WorldPivot");
  put("WorldPivot", modelPivotValue(node), worldPivotProperty, propertyDataType(worldPivotProperty) ?? "CFrame", 3);
}

function numberMember(record: Record<string, unknown> | undefined, name: string, fallback = 0): number {
  const raw = record?.[name];
  return typeof raw === "number" && Number.isFinite(raw) ? raw : fallback;
}

function pascalNumberMember(record: Record<string, unknown> | undefined, lowerName: string, upperName: string, fallback = 0): number {
  return numberMember(record, lowerName, numberMember(record, upperName, fallback));
}

function enumNameFromBytecodeValue(value: unknown, dataType?: string): string {
  const record = recordValue(value);
  if (record?._type === "EnumItem") {
    return String(record.name ?? record.Name ?? "");
  }
  if (typeof value === "string") {
    const enumType = dataType?.startsWith("Enum.") ? dataType : undefined;
    return enumType && value.startsWith(`${enumType}.`) ? value.slice(enumType.length + 1) : value.split(".").pop() ?? value;
  }
  return "";
}

function enumValuesForDataType(dataType: string | undefined, database: RbxDomDatabase, fallbackItems?: string[]): Array<{ name: string; value: number }> | undefined {
  if (!dataType?.startsWith("Enum.")) {
    return undefined;
  }
  const enumType = dataType.slice("Enum.".length);
  const items = database.Enums?.[enumType]?.items;
  if (items) {
    return Object.entries(items)
      .sort((a, b) => a[1] - b[1])
      .map(([name, value]) => ({ name, value }));
  }
  return fallbackItems?.map((name, index) => ({ name, value: index }));
}

export function verdeTypeForValue(value: unknown, dataType?: string): string {
  if (dataType?.startsWith("Enum.")) {
    return dataType;
  }
  switch (dataType) {
    case "Bool":
      return "boolean";
    case "Int32":
    case "Int64":
      return "int";
    case "Float32":
    case "Float64":
    case "Double":
      return "number";
    case "String":
    case "BinaryString":
    case "ProtectedString":
      return "string";
    case "Content":
    case "ContentId":
      return "ContentId";
    case "Vector2":
    case "Vector3":
    case "UDim":
    case "UDim2":
    case "CFrame":
    case "OptionalCFrame":
    case "Color3":
    case "BrickColor":
    case "NumberRange":
    case "NumberSequence":
    case "ColorSequence":
      return dataType === "OptionalCFrame" ? "CFrame" : dataType;
    case "Ref":
    case "Instance":
      return "Ref";
    default:
      break;
  }
  const record = recordValue(value);
  const typeName = record?._type;
  if (typeof typeName === "string") {
    return typeName === "EnumItem" ? dataType ?? "string" : typeName;
  }
  if (typeof value === "boolean") {
    return "boolean";
  }
  if (typeof value === "number") {
    return Number.isInteger(value) ? "int" : "number";
  }
  return "string";
}

function color3ToVerde(value: unknown): { R: number; G: number; B: number } {
  const record = recordValue(value);
  return {
    R: pascalNumberMember(record, "r", "R"),
    G: pascalNumberMember(record, "g", "G"),
    B: pascalNumberMember(record, "b", "B"),
  };
}

function brickColorNumberFromValue(value: unknown, fallback = 194): number {
  const record = recordValue(value);
  const raw = record?.number ?? record?.Number ?? record?.BrickColor ?? value;
  const number = typeof raw === "number" ? raw : Number(raw);
  return Number.isFinite(number) ? Math.trunc(number) : fallback;
}

function brickColorToVerde(value: unknown): { Number: number } {
  return { Number: brickColorNumberFromValue(value) };
}

function refFromText(value: string, normalizePath = false): Record<string, unknown> | null {
  const text = value.trim();
  if (text.length === 0 || /^none|null$/i.test(text)) {
    return null;
  }
  if (text.includes(".")) {
    const pathKey = normalizePath ? refPathKeyFromText(text) : undefined;
    return {
      _type: "Ref",
      pathSegments: pathKey
        ? pathKey.split("\0")
        : text.split(".").map((segment) => segment.trim()).filter((segment) => segment.length > 0),
    };
  }
  return {
    _type: "Ref",
    settingsId: text,
    instanceId: text,
  };
}

function taggedRefRecord(record: Record<string, unknown>): Record<string, unknown> {
  if (record._type === "Ref") {
    return record;
  }
  const legacyRef = recordValue(record.Ref);
  return { _type: "Ref", ...(legacyRef ?? record) };
}

function refToVerde(value: unknown): unknown {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === "string") {
    return refFromText(value, true);
  }
  const record = recordValue(value);
  if (!record) {
    return value;
  }
  return taggedRefRecord(record);
}

export function refRecordFromValue(value: unknown): Record<string, unknown> | undefined {
  if (typeof value === "string") {
    return refFromText(value) ?? undefined;
  }
  const record = recordValue(value);
  if (!record) {
    return undefined;
  }
  if (record._type === "Ref") {
    return record;
  }
  return recordValue(record.Ref);
}

function refPathSegmentsFromRecord(record: Record<string, unknown> | undefined): string[] {
  const segments = Array.isArray(record?.pathSegments) ? record.pathSegments : Array.isArray(record?.PathSegments) ? record.PathSegments : [];
  return segments.map((segment) => String(segment)).filter((segment) => segment.length > 0);
}

function refDisplayText(value: unknown, target?: FileExplorerNode): string {
  if (value === null || value === undefined) {
    return "None";
  }
  if (target) {
    return target.pathSegments.length > 0 ? target.pathSegments.join(".") : target.name;
  }
  const record = refRecordFromValue(value);
  if (!record) {
    return String(value);
  }
  const pathSegments = refPathSegmentsFromRecord(record);
  if (pathSegments.length > 0) {
    return pathSegments.join(".");
  }
  const named = record.name ?? record.Name;
  if (typeof named === "string" && named.length > 0) {
    return named;
  }
  const settingsId = record.settingsId ?? record.instanceId;
  if (typeof settingsId === "string" && settingsId.length > 0) {
    return settingsId;
  }
  const instanceIndex = record.instanceIndex;
  if (typeof instanceIndex === "number" && Number.isFinite(instanceIndex)) {
    return `Instance #${Math.trunc(instanceIndex)}`;
  }
  const referent = record.referent ?? record.ref;
  if (typeof referent === "string" && referent.length > 0) {
    return referent;
  }
  return "None";
}

function brickColorDisplayText(value: unknown): string {
  return String(brickColorNumberFromValue(value));
}

function vector2ToVerde(value: unknown): { X: number; Y: number } {
  const record = recordValue(value);
  return {
    X: pascalNumberMember(record, "x", "X"),
    Y: pascalNumberMember(record, "y", "Y"),
  };
}

function vector3ToVerde(value: unknown): { X: number; Y: number; Z: number } {
  const record = recordValue(value);
  return {
    X: pascalNumberMember(record, "x", "X"),
    Y: pascalNumberMember(record, "y", "Y"),
    Z: pascalNumberMember(record, "z", "Z"),
  };
}

function udimToVerde(value: unknown): { Scale: number; Offset: number } {
  const record = recordValue(value);
  return {
    Scale: pascalNumberMember(record, "scale", "Scale"),
    Offset: pascalNumberMember(record, "offset", "Offset"),
  };
}

function udim2ToVerde(value: unknown): { X: { Scale: number; Offset: number }; Y: { Scale: number; Offset: number } } {
  const record = recordValue(value);
  const x = recordValue(record?.X);
  const y = recordValue(record?.Y);
  return {
    X: {
      Scale: pascalNumberMember(record, "xScale", "XScale", numberMember(x, "Scale")),
      Offset: pascalNumberMember(record, "xOffset", "XOffset", numberMember(x, "Offset")),
    },
    Y: {
      Scale: pascalNumberMember(record, "yScale", "YScale", numberMember(y, "Scale")),
      Offset: pascalNumberMember(record, "yOffset", "YOffset", numberMember(y, "Offset")),
    },
  };
}

function cframeComponents(value: unknown): number[] {
  const record = recordValue(value);
  const components = Array.isArray(record?.components) ? record.components : Array.isArray(record?.Components) ? record.Components : [];
  if (components.length === 0 && record) {
    const position = Array.isArray(record.position) ? record.position : Array.isArray(record.Position) ? record.Position : undefined;
    const orientation = Array.isArray(record.orientation) ? record.orientation : Array.isArray(record.Orientation) ? record.Orientation : undefined;
    if (position || orientation) {
      const row0 = Array.isArray(orientation?.[0]) ? orientation[0] : [1, 0, 0];
      const row1 = Array.isArray(orientation?.[1]) ? orientation[1] : [0, 1, 0];
      const row2 = Array.isArray(orientation?.[2]) ? orientation[2] : [0, 0, 1];
      return [
        typeof position?.[0] === "number" ? position[0] : 0,
        typeof position?.[1] === "number" ? position[1] : 0,
        typeof position?.[2] === "number" ? position[2] : 0,
        typeof row0[0] === "number" ? row0[0] : 1,
        typeof row0[1] === "number" ? row0[1] : 0,
        typeof row0[2] === "number" ? row0[2] : 0,
        typeof row1[0] === "number" ? row1[0] : 0,
        typeof row1[1] === "number" ? row1[1] : 1,
        typeof row1[2] === "number" ? row1[2] : 0,
        typeof row2[0] === "number" ? row2[0] : 0,
        typeof row2[1] === "number" ? row2[1] : 0,
        typeof row2[2] === "number" ? row2[2] : 1,
      ];
    }
  }
  const out = components.map((item) => typeof item === "number" && Number.isFinite(item) ? item : 0);
  while (out.length < 12) {
    out.push(out.length === 3 || out.length === 7 || out.length === 11 ? 1 : 0);
  }
  return out.slice(0, 12);
}

function cframeToVerde(value: unknown): { Position: { X: number; Y: number; Z: number }; Rotation: { X: number; Y: number; Z: number } } {
  const components = cframeComponents(value);
  return {
    Position: { X: components[0] ?? 0, Y: components[1] ?? 0, Z: components[2] ?? 0 },
    Rotation: { X: 0, Y: 0, Z: 0 },
  };
}

function sequenceToVerde(value: unknown, valueKind: "number" | "color"): { Keypoints: unknown[] } {
  const record = recordValue(value);
  const keypoints = Array.isArray(record?.keypoints) ? record.keypoints : Array.isArray(record?.Keypoints) ? record.Keypoints : [];
  return {
    Keypoints: keypoints.map((raw) => {
      const keypoint = recordValue(raw);
      if (valueKind === "color") {
        return {
          Time: pascalNumberMember(keypoint, "time", "Time"),
          Value: color3ToVerde(keypoint?.value ?? keypoint?.color ?? keypoint?.Value),
        };
      }
      return {
        Time: pascalNumberMember(keypoint, "time", "Time"),
        Value: pascalNumberMember(keypoint, "value", "Value"),
        Envelope: pascalNumberMember(keypoint, "envelope", "Envelope"),
      };
    }),
  };
}

function contentToVerdeString(value: unknown): string {
  if (typeof value === "string") return value;
  const record = recordValue(value);
  if (record) {
    for (const key of ["Uri", "uri", "Url", "url"]) {
      const uri = record[key];
      if (typeof uri === "string") return uri;
    }
    return "";
  }
  return String(value);
}

function valueToVerde(value: unknown, type: string, database: RbxDomDatabase, enumItems?: string[]): unknown {
  if (type.startsWith("Enum.")) {
    const name = enumNameFromBytecodeValue(value, type);
    const enumValue = enumValuesForDataType(type, database, enumItems)?.find((item) => item.name === name)?.value ?? 0;
    return { Name: name, Value: enumValue, EnumType: type };
  }
  switch (type) {
    case "boolean":
      return value === true || String(value).toLowerCase() === "true";
    case "number":
    case "int":
    case "float":
    case "double":
      return typeof value === "number" ? value : Number(value) || 0;
    case "Color3":
      return color3ToVerde(value);
    case "BrickColor":
      return brickColorToVerde(value);
    case "Ref":
      return refToVerde(value);
    case "Vector2":
      return vector2ToVerde(value);
    case "Vector3":
      return vector3ToVerde(value);
    case "UDim":
      return udimToVerde(value);
    case "UDim2":
      return udim2ToVerde(value);
    case "CFrame":
    case "OptionalCFrame":
      return cframeToVerde(value);
    case "NumberRange": {
      const record = recordValue(value);
      return { Min: pascalNumberMember(record, "min", "Min"), Max: pascalNumberMember(record, "max", "Max") };
    }
    case "NumberSequence":
      return sequenceToVerde(value, "number");
    case "ColorSequence":
      return sequenceToVerde(value, "color");
    case "ContentId":
    case "string":
      return value === undefined || value === null ? "" : contentToVerdeString(value);
    default:
      return value === undefined || value === null ? "" : typeof value === "object" ? JSON.stringify(value) : value;
  }
}

export function verdePropertyRowsForNode(node: FileExplorerNode, parentName: string, config: ExplorerConfig, resolveReference?: (value: unknown) => FileExplorerNode | undefined): VerdePropertiesData {
  const database = loadRbxDomDatabase(config) ?? {};
  const metadataLocked = node.kind === "service" || isProtectedStarterPlayerContainer(node);
  const properties: VerdePropertyInfo[] = [
    { name: "Name", type: "string", value: node.name, category: "Data", layoutOrder: -3, isReadOnly: metadataLocked },
    { name: "ClassName", type: "string", value: node.className, category: "Data", layoutOrder: -2, isReadOnly: true },
    {
      name: "Parent",
      type: "Ref",
      value: parentName || "game",
      category: "Data",
      layoutOrder: -1,
      isInstanceReference: true,
      referencedInstanceId: node.parentTreeId ?? undefined,
      referencedInstanceName: parentName || "game",
    },
  ];
  for (const row of propertyRowsForNode(node, config)) {
    const type = verdeTypeForValue(row.value, row.dataType);
    const enumValues = enumValuesForDataType(row.dataType, database, row.enumItems);
    const value = valueToVerde(row.value, type, database, row.enumItems);
    const propertyInfo: VerdePropertyInfo = {
      name: row.name,
      displayName: row.displayName,
      type,
      value,
      category: row.category || "Data",
      layoutOrder: row.order,
      isEnum: type.startsWith("Enum."),
      enumValues,
      isReadOnly: row.readonly,
      uiMinimum: row.uiMinimum,
      uiMaximum: row.uiMaximum,
      uiNumTicks: row.uiNumTicks,
      sliderScaling: row.sliderScaling,
    };
    if (type === "BrickColor") {
      propertyInfo.displayValue = brickColorDisplayText(value);
    }
    if (type === "Ref") {
      const target = resolveReference?.(row.value) ?? resolveReference?.(value);
      const displayValue = refDisplayText(row.value, target);
      propertyInfo.isInstanceReference = true;
      propertyInfo.displayValue = displayValue;
      if (target) {
        propertyInfo.referencedInstanceId = target.treeId;
        propertyInfo.referencedInstanceName = displayValue;
        propertyInfo.referencedInstanceClass = target.className;
      } else if (displayValue !== "None") {
        propertyInfo.referencedInstanceName = displayValue;
      }
    }
    properties.push(propertyInfo);
  }
  return {
    properties,
    tags: searchTagsFromNode(node),
    attributes: Object.entries(node.attributes)
      .filter(([name]) => !name.startsWith("RBX_"))
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([name, value]) => {
        const type = verdeTypeForValue(value);
        return { name, type, value: valueToVerde(value, type, database) };
      }),
  };
}

function bytecodeColor3(value: Record<string, unknown> | undefined): unknown {
  return { _type: "Color3", r: numberMember(value, "R"), g: numberMember(value, "G"), b: numberMember(value, "B") };
}

function bytecodeBrickColor(value: unknown, currentValue: unknown): unknown {
  return { _type: "BrickColor", number: brickColorNumberFromValue(value, brickColorNumberFromValue(currentValue)) };
}

function bytecodeRef(value: unknown, currentValue: unknown): unknown {
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value === "string") {
    return refFromText(value);
  }
  const record = recordValue(value);
  if (!record) {
    return currentValue;
  }
  return taggedRefRecord(record);
}

function bytecodeVector2(value: Record<string, unknown> | undefined): unknown {
  return { _type: "Vector2", x: numberMember(value, "X"), y: numberMember(value, "Y") };
}

function bytecodeVector3(value: Record<string, unknown> | undefined): unknown {
  return { _type: "Vector3", x: numberMember(value, "X"), y: numberMember(value, "Y"), z: numberMember(value, "Z") };
}

function bytecodeUdim(value: Record<string, unknown> | undefined): unknown {
  return { _type: "UDim", scale: numberMember(value, "Scale"), offset: numberMember(value, "Offset") };
}

function bytecodeUdim2(value: Record<string, unknown> | undefined): unknown {
  const x = recordValue(value?.X);
  const y = recordValue(value?.Y);
  return {
    _type: "UDim2",
    xScale: numberMember(x, "Scale"),
    xOffset: numberMember(x, "Offset"),
    yScale: numberMember(y, "Scale"),
    yOffset: numberMember(y, "Offset"),
  };
}

function bytecodeCFrame(value: Record<string, unknown> | undefined, currentValue: unknown): unknown {
  const components = cframeComponents(currentValue);
  const position = recordValue(value?.Position);
  if (position) {
    components[0] = numberMember(position, "X");
    components[1] = numberMember(position, "Y");
    components[2] = numberMember(position, "Z");
  }
  return { _type: "CFrame", components };
}

function bytecodeSequence(value: Record<string, unknown> | undefined, valueKind: "number" | "color"): unknown {
  const keypoints = Array.isArray(value?.Keypoints) ? value.Keypoints : [];
  return {
    _type: valueKind === "color" ? "ColorSequence" : "NumberSequence",
    keypoints: keypoints.map((raw) => {
      const keypoint = recordValue(raw);
      if (valueKind === "color") {
        return {
          time: pascalNumberMember(keypoint, "time", "Time"),
          value: bytecodeColor3(recordValue(keypoint?.Value)),
        };
      }
      return {
        time: pascalNumberMember(keypoint, "time", "Time"),
        value: pascalNumberMember(keypoint, "value", "Value"),
        envelope: pascalNumberMember(keypoint, "envelope", "Envelope"),
      };
    }),
  };
}

export function bytecodeValueFromVerde(value: unknown, type: string | undefined, currentValue: unknown): unknown {
  if (type?.startsWith("Enum.")) {
    const record = recordValue(value);
    return {
      _type: "EnumItem",
      enumType: type,
      name: String(record?.EnumName ?? record?.Name ?? value ?? ""),
    };
  }
  const record = recordValue(value);
  switch (type) {
    case "BrickColor":
      return bytecodeBrickColor(value, currentValue);
    case "Ref":
      return bytecodeRef(value, currentValue);
    case "Color3":
      return bytecodeColor3(record);
    case "Vector2":
      return bytecodeVector2(record);
    case "Vector3":
      return bytecodeVector3(record);
    case "UDim":
      return bytecodeUdim(record);
    case "UDim2":
      return bytecodeUdim2(record);
    case "CFrame":
    case "OptionalCFrame":
      return bytecodeCFrame(record, currentValue);
    case "NumberRange":
      return { _type: "NumberRange", min: numberMember(record, "Min"), max: numberMember(record, "Max") };
    case "NumberSequence":
      return bytecodeSequence(record, "number");
    case "ColorSequence":
      return bytecodeSequence(record, "color");
    default:
      return value;
  }
}

export function defaultAttributeValue(type: string): unknown {
  switch (type) {
    case "number":
      return 0;
    case "boolean":
      return false;
    case "Color3":
      return { _type: "Color3", r: 0, g: 0, b: 0 };
    case "Vector2":
      return { _type: "Vector2", x: 0, y: 0 };
    case "Vector3":
      return { _type: "Vector3", x: 0, y: 0, z: 0 };
    case "UDim":
      return { _type: "UDim", scale: 0, offset: 0 };
    case "UDim2":
      return { _type: "UDim2", xScale: 0, xOffset: 0, yScale: 0, yOffset: 0 };
    case "NumberRange":
      return { _type: "NumberRange", min: 0, max: 0 };
    case "NumberSequence":
      return { _type: "NumberSequence", keypoints: [{ time: 0, value: 0, envelope: 0 }, { time: 1, value: 0, envelope: 0 }] };
    case "ColorSequence":
      return { _type: "ColorSequence", keypoints: [{ time: 0, value: { _type: "Color3", r: 0, g: 0, b: 0 } }, { time: 1, value: { _type: "Color3", r: 1, g: 1, b: 1 } }] };
    default:
      return "";
  }
}

function searchValueText(value: unknown): string {
  if (value === null || value === undefined) {
    return "";
  }
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function clonePathKey(pathSegments: unknown): string | undefined {
  if (!Array.isArray(pathSegments)) {
    return undefined;
  }
  const parts = pathSegments.map((segment) => String(segment));
  return parts.length > 0 ? parts.join("\0") : undefined;
}

export function refPathKeyFromSegments(pathSegments: unknown): string | undefined {
  if (!Array.isArray(pathSegments)) {
    return undefined;
  }
  const parts = pathSegments.map((segment) => String(segment).trim()).filter((segment) => segment.length > 0);
  const normalized = parts[0]?.toLowerCase() === "game" ? parts.slice(1) : parts;
  return clonePathKey(normalized);
}

function refPathKeyFromText(value: string): string | undefined {
  const text = value.trim();
  if (text.length === 0 || /^none|null$/i.test(text)) {
    return undefined;
  }
  return refPathKeyFromSegments(text.split("."));
}

export function refTargetFromObject(object: Record<string, unknown>): { settingsId?: string; index?: number; pathKey?: string } {
  const instanceIndex = typeof object.instanceIndex === "number" ? object.instanceIndex : undefined;
  const zeroIndex = instanceIndex !== undefined && Number.isFinite(instanceIndex) ? Math.trunc(instanceIndex) - 1 : undefined;
  const settingsId = typeof object.settingsId === "string"
    ? object.settingsId
    : typeof object.instanceId === "string"
      ? object.instanceId
      : undefined;
  return {
    settingsId,
    index: zeroIndex !== undefined && zeroIndex >= 0 ? zeroIndex : undefined,
    pathKey: refPathKeyFromSegments(object.pathSegments ?? object.PathSegments),
  };
}

export function appendRecordAssignments(args: string[], flag: string, record: Record<string, unknown>): void {
  for (const [name, value] of Object.entries(record)) {
    const encoded = JSON.stringify(value);
    if (encoded !== undefined) {
      args.push(flag, `${name}=${encoded}`);
    }
  }
}

export function searchTagsFromNode(node: FileExplorerNode): string[] {
  const raw = node.properties.Tags ?? node.attributes.Tags ?? node.properties.tags ?? node.attributes.tags;
  if (Array.isArray(raw)) {
    return raw.map((value) => searchValueText(value)).filter((value) => value.length > 0);
  }
  const text = searchValueText(raw);
  return text ? [text] : [];
}
