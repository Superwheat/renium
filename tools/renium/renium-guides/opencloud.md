# Open Cloud and creator assets

Cloud commands run without Studio. Store a key once with `rbx oc key add NAME` (the key is read from a hidden prompt, never from arguments) and every `rbx oc` command uses it; `--key NAME` picks another stored key, `ROBLOX_API_KEY` or `--key-env ENV` / `--oauth-env ENV` override the store. Keys are kept per user outside every project (DPAPI on Windows, the Keychain on macOS). Never put credentials in arguments, project files or shell profiles.

```powershell
rbx oc key add studio
rbx oc key list
rbx oc key
```

`key` alone reports the active key's permissions without exposing the secret. Roblox enforces owner permissions, scopes, and targets; Renium does not widen access or switch credentials.

## Find and pull an experience

```powershell
rbx oc games
rbx oc games "Brainrot Town"
rbx oc fetch "Brainrot Town" -r ./BrainrotTown
rbx oc fetch --universe 8108639406 -o brainrot.rbxl
rbx oc place history
rbx oc fetch --version 940 -o old.rbxl
rbx oc fetch 112966546347918 -o other.rbxl     # a place id or universe id; the positional wins over the bound place
```

`games` lists what the key can reach: the universes it is scoped to plus the public experiences of the key's user and groups; a name matches ignoring case, emoji and punctuation. The experience's main icon and thumbnails have no API-key route (Open Cloud proxies only the badge icon of the legacy publish API); set them in Creator Hub. `fetch` downloads the experience's root place and, with `-r DIR`, imports it into that project (creating it) so the files are ready to open with `rbx so`. Private experiences the key is not scoped to need `--universe ID` or `--place-id ID`. Without `--version`, `fetch` downloads the newest published version by number (the result says which) rather than the delivery copy, which can lag a publish. Older versions of the bound place come from Roblox: `place history` lists its saved versions (newest first, with times and authors) and `fetch --version N` downloads one for `q`, `v` or `cmp`. Place files found elsewhere on the machine are not versions of the project unless the user says so.

Explicit fetch names, `--universe`, and `--place-id` override the bound project. Numeric names try a universe first, then a place only when no universe exists; lookup errors are reported. Other commands use the project's universe/place IDs by default. Otherwise put `--universe ID` and `--place-id ID` before the resource.
Public reads can explicitly use `--anonymous`; authenticated requests never fall back to anonymous access.

## Open Cloud resource operations

See [Open Cloud resource operations](opencloud-resources.md).

## Open Cloud assets and analytics

See [Open Cloud assets and analytics](opencloud-assets.md).
