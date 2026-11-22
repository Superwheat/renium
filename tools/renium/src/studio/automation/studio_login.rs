//! Requests to Roblox under the login of a connected Studio, for the commands
//! that work without an API key.
use super::*;
use crate::automation::property_access::Scope;

/// The Studio Edit window whose login a request goes under.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct StudioLogin<'a> {
    bridge: &'a BridgeServer,
    scope: Scope,
    declared: Value,
}

impl<'a> StudioLogin<'a> {
    /// The Edit window `bridge` is connected to, with the HTTP enum items its
    /// engine declares.
    #[cfg(windows)]
    pub(crate) fn connect(bridge: &'a BridgeServer) -> Result<Self> {
        let pin = bridge.runtime_pin_for_selector(BridgeTarget::Edit, None)?;
        let scope = Scope {
            project: String::new(),
            pid: bridge.studio_pid_for_runtime(BridgeTarget::Edit, &pin.runtime_id)?,
            runtime: pin.runtime_id,
        };
        let declared = property_access::http_enum_items(bridge, &scope)?;
        Ok(Self {
            bridge,
            scope,
            declared,
        })
    }

    #[cfg(not(windows))]
    pub(crate) fn connect(_bridge: &'a BridgeServer) -> Result<Self> {
        bail!("Requests under the Studio login need Studio on Windows")
    }

    /// Posts `body` to a Roblox address and returns the answer's text; an
    /// HTTP error is an error that carries what Roblox answered.
    #[cfg(windows)]
    pub(crate) fn post(&self, url: &str, content_type: &str, body: &[u8]) -> Result<String> {
        self.window_title()?;
        answered(&crate::studio::native::serializer::studio_post(
            self.scope.pid,
            (url, content_type),
            body,
            &self.declared,
        )?)
    }

    /// Gets a Roblox address and returns the answer's text.
    #[cfg(windows)]
    pub(crate) fn get(&self, url: &str) -> Result<String> {
        use crate::studio::native::serializer;
        let title = self.window_title()?;
        let (class, function) = ("HttpRbxApiService", "GetAsyncFullUrl");
        let arguments = [json!(url)];
        serializer::validate_function_arguments(class, function, &arguments, &self.declared)?;
        let mut native = serializer::prepare_property(
            self.scope.pid,
            &title,
            &[class.to_string()],
            &[],
            "Name",
            Duration::from_secs(3),
        )?;
        native.prepare_function(self.scope.pid, &title, function, &arguments)?;
        answered(&native.call_function_answer(Duration::from_secs(30))?)
    }

    #[cfg(not(windows))]
    pub(crate) fn post(&self, _url: &str, _content_type: &str, _body: &[u8]) -> Result<String> {
        bail!("Requests under the Studio login need Studio on Windows")
    }

    #[cfg(not(windows))]
    pub(crate) fn get(&self, _url: &str) -> Result<String> {
        bail!("Requests under the Studio login need Studio on Windows")
    }

    /// The title of the window the requests go through, while the Studio
    /// this login was read from is still the one connected.
    #[cfg(windows)]
    fn window_title(&self) -> Result<String> {
        let info = self
            .bridge
            .cached_bridge_info_for_target(BridgeTarget::Edit)?;
        let pid = self
            .bridge
            .studio_pid_for_runtime(BridgeTarget::Edit, &self.scope.runtime)?;
        anyhow::ensure!(
            info.runtime_id == self.scope.runtime
                && pid == self.scope.pid
                && self.declared.is_object(),
            "The Studio the request was prepared for is no longer connected"
        );
        crate::studio::native::serializer::target_name(self.scope.pid, &info.place_name)
    }
}

/// The text of a yielding call's answer, or what Roblox refused it with.
#[cfg(any(windows, test))]
fn answered(answer: &Value) -> Result<String> {
    let text = answer["text"].as_str().unwrap_or_default();
    anyhow::ensure!(answer["httpError"] != true, "Roblox answered {text}");
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_http_error_is_an_error_with_what_roblox_answered() {
        let ok = json!({ "httpError": false, "text": "{\"done\":true}" });
        assert_eq!(answered(&ok).unwrap(), "{\"done\":true}");
        let refused = json!({ "httpError": true, "text": "HTTP 400 (Bad Request)" });
        assert_eq!(
            answered(&refused).unwrap_err().to_string(),
            "Roblox answered HTTP 400 (Bad Request)"
        );
    }
}
