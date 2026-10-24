//! Explicitly owned Studio acceptance fixture; never part of ordinary test runs.
use super::*;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use std::os::windows::process::CommandExt;

struct Driver {
    binary: std::path::PathBuf,
    project: std::path::PathBuf,
    place: String,
}

impl Driver {
    fn run(&self, arguments: &[&str]) -> Result<Value> {
        let output = std::process::Command::new(&self.binary)
            .current_dir(&self.project)
            .args(["--place", &self.place])
            .args(arguments)
            .creation_flags(0x08000000)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Owned fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout)
            .with_context(|| String::from_utf8_lossy(&output.stderr).into_owned())?;
        anyhow::ensure!(
            result["ok"] != false,
            "Owned fixture command failed: {result}"
        );
        Ok(result)
    }
    fn eval(&self, role: &str, code: &str) -> Result<Value> {
        let mut args = vec!["l"];
        args.push(if role.is_empty() { "--edit" } else { role });
        args.push(code);
        Ok(self.run(&args)?["results"][0].clone())
    }
    fn launch(&self, owner: &str, nonce: &str) -> Result<Value> {
        // Game scripts return their first-call observations through EndTest.
        // No second automation command competes with this running executor.
        let code = format!(
            r#"
            game:SetAttribute("__ReniumLaunchNonce", "{nonce}")
            game:SetAttribute("__ReniumEditRuntimeId", "{owner}")
            return game:GetService("StudioTestService"):ExecutePlayModeAsync({{
                __renium={{nonce="{nonce}",editRuntimeId="{owner}",placeName=game.Name}}
            }})
        "#
        );
        Ok(self.run(&["l", "--edit", "--timeout", "120", &code])?["results"][0].clone())
    }
}

fn arrival_fixture() -> Result<Vec<u8>> {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
    let storage = dom.insert(dom.root_ref(), InstanceBuilder::new("ReplicatedStorage"));
    dom.insert(
        storage,
        InstanceBuilder::new("RemoteEvent").with_name("ReniumFirstCallbacks"),
    );
    // Incompressible fixture content exceeds the lab source clone's buffer;
    // this exercises engine allocation rather than a smaller-buffer overwrite.
    let mut random = 0x9e3779b9u32;
    let payload: String = (0..2 * 1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            char::from(32 + (random % 95) as u8)
        })
        .collect();
    let server_storage = dom.insert(dom.root_ref(), InstanceBuilder::new("ServerStorage"));
    dom.insert(
        server_storage,
        InstanceBuilder::new("StringValue")
            .with_name("ReniumLargeLoadFixture")
            .with_property("Value", payload),
    );
    for (service, class, code) in [
        (
            "ServerScriptService",
            "Script",
            r#"
            local players = game:GetService("Players")
            assert(#game:GetService("ServerStorage").ReniumLargeLoadFixture.Value == 2*1024*1024)
            local first
            local function capture(player)
                first = player:GetJoinData()
            end
            players.PlayerAdded:Connect(capture)
            for _, player in players:GetPlayers() do capture(player) end
            game:GetService("ReplicatedStorage").ReniumFirstCallbacks.OnServerEvent:Connect(function(_, client)
                game:GetService("StudioTestService"):EndTest({server={place=game.PlaceId,first=first},client=client})
            end)
            task.delay(25,function() game:GetService("StudioTestService"):EndTest({error="First client callback did not arrive"}) end)
        "#,
        ),
        (
            "ReplicatedFirst",
            "LocalScript",
            r#"
            local service = game:GetService("TeleportService")
            local data = service:GetLocalPlayerTeleportData()
            local players = game:GetService("Players")
            local player = players.LocalPlayer or players.PlayerAdded:Wait()
            local first = {join=player:GetJoinData(),data=data}
            game:GetService("ReplicatedStorage"):WaitForChild("ReniumFirstCallbacks"):FireServer({place=game.PlaceId,first=first})
        "#,
        ),
    ] {
        let parent = dom.insert(dom.root_ref(), InstanceBuilder::new(service));
        dom.insert(
            parent,
            InstanceBuilder::new(class)
                .with_name("ReniumArrivalFixture")
                .with_property("Source", code),
        );
    }
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, dom.root().children())?;
    Ok(bytes)
}

#[test]
#[ignore = "requires explicit owned Studio PID, lab project, alias and matching CLI"]
fn native_teleport_result_and_first_game_callbacks() -> Result<()> {
    let driver = Driver {
        binary: std::env::var("RENIUM_TELEPORT_TEST_CLI")?.into(),
        project: std::env::var("RENIUM_TELEPORT_TEST_PROJECT")?.into(),
        place: std::env::var("RENIUM_TELEPORT_TEST_PLACE")?,
    };
    let pid: u32 = std::env::var("RENIUM_TELEPORT_TEST_PID")?.parse()?;
    let destination: i64 = std::env::var("RENIUM_TELEPORT_TEST_DESTINATION")?.parse()?;
    let status = driver.run(&["status"])?;
    anyhow::ensure!(
        status["playState"] == "stopped",
        "Stop the owned fixture before this test"
    );
    let owner = status["selected"]
        .as_str()
        .context("No selected Edit runtime")?;
    anyhow::ensure!(
        status["clients"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["role"] == "edit" && r["pid"] == pid)),
        "The lab binding does not select the owned PID"
    );
    let before = driver.eval(
        "",
        r#"return {id=game:GetDebugId(),place=game.PlaceId,universe=game.GameId}"#,
    )?;
    let source = (
        before["place"].as_i64().context("Source place missing")?,
        before["universe"]
            .as_i64()
            .context("Source universe missing")?,
    );
    let nonce = format!(
        "fixture-{}-{}",
        std::process::id(),
        crate::app::update::process_start_identity(std::process::id())
            .context("Test process ended")?
    );
    let next = format!("{nonce}-arrival");
    let name = "__ReniumNativeSourceFixture";
    let cleanup = crate::system::files::OnDrop::new(|| {
        let _ = driver.run(&["play", "--stop"]);
        let _ = driver.eval("", &format!(r#"
            local s=game:GetService("ServerScriptService"):FindFirstChild("{name}")
            if s and s:GetAttribute("Owner")=="{nonce}" then s:Destroy() end
            if game:GetAttribute("__ReniumLaunchNonce")=="{nonce}" or game:GetAttribute("__ReniumLaunchNonce")=="{next}" then
                game:SetAttribute("__ReniumLaunchNonce",nil)
                game:SetAttribute("__ReniumEditRuntimeId",nil)
            end
            return true
        "#));
    });
    let source_code = format!(
        r#"
        local service=game:GetService("TeleportService")
        local relay=Instance.new("BindableFunction")
        relay.Name="__ReniumNativeFixtureRelay"
        relay:SetAttribute("__ReniumNative","{nonce}")
        relay.OnInvoke=function(place,options,player)
            local data=options:GetTeleportData()
            return {{SourcePlaceId=game.PlaceId,SourceGameId=game.GameId,TeleportData=data,Members={{player.UserId}}}},data,"1"
        end
        relay.Parent=game:GetService("ServerStorage")
        local function travel(player)
            local options=Instance.new("TeleportOptions")
            options:SetTeleportData({{message="first-callback",nested={{number=17,enabled=true}}}})
            local ok,result=pcall(function() return service:TeleportAsync({destination},{{player}},options) end)
            game:GetService("StudioTestService"):EndTest({{
                ok=ok,kind=typeof(result),class=ok and result.ClassName or tostring(result),
                private=ok and result.PrivateServerId or nil,reserved=ok and result.ReservedServerAccessCode or nil
            }})
        end
        game:GetService("Players").PlayerAdded:Connect(travel)
        task.delay(25,function() game:GetService("StudioTestService"):EndTest({{error="Source PlayerAdded did not complete"}}) end)
    "#
    );
    driver.eval(
        "",
        &format!(
            r#"
        local parent=game:GetService("ServerScriptService")
        assert(not parent:FindFirstChild("{name}"))
        local s=Instance.new("Script") s.Name="{name}" s:SetAttribute("Owner","{nonce}")
        s.Source=[==[{source_code}]==] s.Parent=parent return true
    "#
        ),
    )?;
    let title = crate::studio::input::studio_window_title(pid)?;
    let mut dispatch = dispatch::NativeTeleportDispatch::before_play(pid, &title, &nonce)?;
    println!("Native source interception armed; launching the owned fixture");
    let result = driver.launch(owner, &nonce)?;
    anyhow::ensure!(
        result
            == json!({"ok":true,"kind":"Instance","class":"TeleportAsyncResult","private":"","reserved":""}),
        "Unexpected native result: {result}"
    );
    anyhow::ensure!(
        dispatch.accepted(1, destination)?,
        "The native source callback was not accepted"
    );
    let mut arrival = join::NativePlayJoin::prepare(pid, &title, true)?;
    dispatch.advance(destination, &next)?;
    arrival.arm(
        source,
        destination,
        &next,
        dispatch.token(),
        1,
        Some(&dispatch),
    )?;
    let mut load = loader::NativePlayLoad::arm(
        pid,
        &title,
        source,
        (destination, source.1),
        &arrival_fixture()?,
        &next,
        Some(&dispatch),
    )?;
    println!("Native source result: {result}");
    let observations = driver.launch(owner, &next)?;
    println!("First server/client game callbacks captured");
    for role in ["server", "client"] {
        let first = &observations[role];
        let join = if role == "client" {
            &first["first"]["join"]
        } else {
            &first["first"]
        };
        anyhow::ensure!(
            first["place"] == destination
                && join["SourcePlaceId"] == source.0
                && join["SourceGameId"] == source.1,
            "First {role} callback had the wrong native identity: {first}"
        );
        let data = json!({"message":"first-callback","nested":{"number":17,"enabled":true}});
        anyhow::ensure!(
            join["TeleportData"] == data,
            "First {role} callback missed native join data: {first}"
        );
        if role == "client" {
            anyhow::ensure!(
                first["first"]["data"] == data,
                "The first client script missed local teleport data"
            );
        }
    }
    anyhow::ensure!(
        load.consumed()?,
        "The instrumented destination was not consumed"
    );
    let after = driver.eval(
        "",
        "return {id=game:GetDebugId(),place=game.PlaceId,universe=game.GameId}",
    )?;
    anyhow::ensure!(after == before, "The original Edit model changed: {after}");
    drop(cleanup);
    println!(
        "Native result, first server/client join callbacks, local teleport data and original Edit restoration passed"
    );
    Ok(())
}
