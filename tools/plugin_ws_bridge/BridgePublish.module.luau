local BridgePublish = {}

function BridgePublish.run(params, context)
	assert(params.runtimeId == context.runtimeId, "Studio changed; select the current Edit runtime")
	assert(not context.isPlayModeRunning(), "Publishing requires Edit mode; stop the playtest first")
	local placeId, gameId = game.PlaceId, game.GameId
	assert(placeId > 0 and gameId > 0, "This Studio place is unpublished; publish it once from Studio to create its destination")
	assert(params.placeId == placeId and params.gameId == gameId, "Publish destination does not match the selected Studio place")
	local result = {
		ok = true,
		published = false,
		dryRun = params.dryRun == true,
		source = "studio",
		runtimeId = context.runtimeId,
		gameId = gameId,
		placeId = placeId,
		url = `https://www.roblox.com/games/{placeId}`,
	}
	if result.dryRun then
		return result
	end
	context.assertRequestLeaseActive()
	context.assertSessionOwnership()
	local ok, message = pcall(function()
		game:GetService("AssetService"):SavePlaceAsync({ PlaceId = placeId, SaveWithoutPublish = false })
	end)
	if not ok then
		return {
			ok = false,
			error = `Studio publish failed: {tostring(message)}. Save Place API must be enabled for this place, with no active Team Create session. Check Version History before retrying.`,
			gameId = gameId,
			placeId = placeId,
		}
	end
	result.published = true
	return result
end

return BridgePublish
