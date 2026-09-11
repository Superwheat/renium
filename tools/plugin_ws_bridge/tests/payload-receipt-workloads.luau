local Overlay = require("../BridgeReferenceOverlay.module")

local function instance(className, name, children)
	local value = { ClassName = className, Name = name, children = children or {} }
	function value:GetChildren()
		return self.children
	end
	for _, child in ipairs(value.children) do
		child.Parent = value
	end
	return value
end

local left = instance("Folder", "Duplicate", { instance("StringValue", "Value") })
local right = instance("Folder", "Duplicate", { instance("NumberValue", "Value") })
local root = instance("Folder", "Wrapper", { left, right })
local strings = { "Folder", "Wrapper", "Duplicate", "StringValue", "NumberValue", "Value" }
local rows = { { 0, 1, 2 }, { 0, 2, 1 }, { 3, 5, 0 }, { 0, 2, 1 }, { 4, 5, 0 } }
local nodes = buffer.create(#rows * 12)
for index, row in ipairs(rows) do
	for field, value in ipairs(row) do
		buffer.writeu32(nodes, (index - 1) * 12 + (field - 1) * 4, value)
	end
end
local structure = { nodes = nodes, strings = strings }
assert(Overlay.matchesPayloadStructure(root, structure))
left.Name = "Different"
assert(not Overlay.matchesPayloadStructure(root, structure), "renames must retain readback")
left.Name = "Duplicate"
root.children = { right, left }
assert(not Overlay.matchesPayloadStructure(root, structure), "duplicate sibling identity order matters")
root.children = { left, right }
right.children[1].Parent = left
assert(not Overlay.matchesPayloadStructure(root, structure), "parent identity must match")
right.children[1].Parent = right
right.children[1].ClassName = "StringValue"
assert(not Overlay.matchesPayloadStructure(root, structure), "complete class receipt is required")
right.children[1].ClassName = "NumberValue"
table.insert(right.children, instance("Folder", "Extra"))
assert(not Overlay.matchesPayloadStructure(root, structure), "extra instances must retain readback")
table.remove(right.children)
local saved = right.children[1]
right.children = {}
assert(not Overlay.matchesPayloadStructure(root, structure), "missing instances must retain readback")
right.children = { saved }
for _, bytes in { 0, 1, buffer.len(nodes) - 1, buffer.len(nodes) + 12 } do
	local malformed = buffer.create(bytes)
	buffer.copy(malformed, 0, nodes, 0, math.min(bytes, buffer.len(nodes)))
	assert(not Overlay.matchesPayloadStructure(root, { nodes = malformed, strings = strings }))
end
buffer.writeu32(nodes, 0, #strings)
assert(not Overlay.matchesPayloadStructure(root, structure), "invalid dictionary indices must retain readback")
buffer.writeu32(nodes, 0, 0)
assert(Overlay.matchesPayloadStructure(root, structure))
print("Payload receipts: exact hierarchy, duplicate siblings, class/name/parent and incomplete receipt fallback passed")
