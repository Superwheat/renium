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

function safeObject(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function safeArray(value) {
  return Array.isArray(value) ? value : [];
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

function normalizeApiDump(raw, runtime = false) {
  const record = safeObject(raw);
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
        ...(!runtime ? {
          Scriptability: typeof member.Scriptability === "string" ? member.Scriptability : undefined,
          SourceKind: "api-dump",
        } : {}),
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
      ...(runtime ? { DefaultProperties: {} } : {}),
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
  return { Classes: classes, Enums: enums };
}

module.exports = {
  MODEL_PIVOT_CLASSES,
  WORKSPACE_HIDDEN_STUDIO_PROPERTIES,
  WORKSPACE_VISIBLE_NON_SERIALIZED_PROPERTIES,
  WORKSPACE_VISIBLE_SERVICE_REF_PROPERTIES,
  WORKSPACE_SERVER_AUTHORITY_PROPERTIES,
  normalizeApiDump,
};
