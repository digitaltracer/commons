# Releasing the Mac app

`cargo xtask release` builds a universal Commons.app (Apple silicon and Intel), signs it with your Developer ID and the hardened runtime, notarizes and staples it, and wraps it in a notarized, stapled `.dmg`. It also writes the files the in-app updater reads. It publishes nothing: you upload the files to a GitHub release yourself.

## One-time setup

1. **Developer ID certificate.** In Xcode → Settings → Accounts → Manage Certificates, add a "Developer ID Application" certificate. It lands in your login keychain. Check it, and note the exact name:

   ```sh
   security find-identity -v -p codesigning
   # "Developer ID Application: Your Name (TEAMID)"
   ```

   Export the name in your shell profile. It is not a secret.

   ```sh
   export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
   ```

2. **Notary credentials.** Create an app-specific password at account.apple.com → Sign-In and Security → App-Specific Passwords. Then store it in the keychain under the profile name the release uses:

   ```sh
   xcrun notarytool store-credentials commons-notary --apple-id you@example.com --team-id TEAMID
   ```

   Paste the password at the prompt. Don't pass `--password` on the command line, or it ends up in your shell history. An App Store Connect API key works too: use `--key`, `--key-id` and `--issuer` instead.

3. **Updater signing key.** Updates are signed with their own key. This key is separate from Apple's signing:

   ```sh
   mkdir -p ~/.tauri
   cargo tauri signer generate -w ~/.tauri/commons-updater.key
   chmod 600 ~/.tauri/commons-updater.key
   ```

   Choose a password and keep it, and a backup of the key, in your password manager. **If the key or its password is lost, installed apps can never update again**, and every user has to download the new dmg by hand. The public half, `~/.tauri/commons-updater.key.pub`, goes in `apps/desktop/tauri.conf.json` under `plugins.updater.pubkey`. It's already there for the current key, and only needs changing if you replace the key. To keep the key somewhere else, set `TAURI_SIGNING_PRIVATE_KEY_PATH`.

4. **Toolchain.** Besides the development tools in the README, a universal build needs the Intel target:

   ```sh
   rustup target add x86_64-apple-darwin
   ```

The release checks all four before it builds, and stops with the fix if one is missing.

## Each release

1. **Bump the version** in `apps/desktop/Cargo.toml`. That is the only place. `tauri.conf.json` has no version, so Tauri uses the crate's, and the release, `latest.json` and the update signature all take it from there. The updater only installs a version greater than the running one. Commit the bump.

2. **Build:**

   ```sh
   cargo xtask release
   ```

   Run it in your own terminal. Near the end, the signer asks for the updater key's password. If you would rather export `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` for the run, do it in that terminal only, never in a file. The build takes a few minutes, and each of the two notarizations (the app, then the dmg) usually takes a few more.

   The output goes to `target/release-mac/<version>/`:

   | File | What it is |
   |---|---|
   | `Commons.app` | The signed, notarized, stapled app |
   | `Commons_<version>_universal.dmg` | The download for new users: signed, notarized, stapled |
   | `Commons_<version>_universal.app.tar.gz` | The update the installed app downloads |
   | `Commons_<version>_universal.app.tar.gz.sig` | Its updater signature (also inside `latest.json`) |
   | `latest.json` | The update manifest the app checks |

3. **Verify.** The release has already run these checks, and they must pass before you publish:

   ```sh
   cd target/release-mac/<version>
   spctl -a -vv -t exec Commons.app                    # accepted, source=Notarized Developer ID
   spctl -a -vv -t open --context context:primary-signature Commons_*_universal.dmg
   xcrun stapler validate Commons.app
   xcrun stapler validate Commons_*_universal.dmg
   ```

   Open the dmg, drag Commons to a scratch folder, launch it, and sign in.

4. **Publish** on GitHub as release `v<version>`. The tag name matters: `latest.json` points at `releases/download/v<version>/…`.

   ```sh
   gh release create v<version> --title "Commons <version>" --notes "…" \
     target/release-mac/<version>/Commons_<version>_universal.dmg \
     target/release-mac/<version>/Commons_<version>_universal.app.tar.gz \
     target/release-mac/<version>/latest.json
   ```

   Installed apps check `https://github.com/digitaltracer/commons/releases/latest/download/latest.json`. That URL always serves the newest release that is neither a draft nor a prerelease. So **publishing the release is what ships the update**. To stage one, create it with `--draft`, check the files, then publish it. Every GitHub release in this repo must carry a `latest.json`, including any future server releases. Otherwise the "latest" link breaks updates.

5. **Check the update.** An app running the previous version shows "Update available" within 6 hours, or at its next launch.

## How the app updates

- Only release builds check. Debug builds (`cargo xtask app`, `cargo xtask dev`) never register the updater.
- The app checks at launch, then every 6 hours. When an update is found, it is downloaded and its signature checked against the public key built into the app, all in the background.
- The app then shows "Update available: Commons <version>" with **Restart to update** and **Later**. Nothing installs, and the app never restarts, until the user clicks Restart to update. Then the new app replaces the old bundle and relaunches. Later hides the prompt until the next launch.
- The update replaces the app where it is, so the app must be somewhere the user can write, like `/Applications`. macOS runs a downloaded app that was never moved out of Downloads or the dmg from a read-only, translocated copy. That copy can't update, and the prompt then reports "The update could not be installed". Moving the app to Applications fixes it.
- `requireSignedVersion` is on: the signature carries the version it was signed for. A tampered `latest.json` therefore can't pass off an old release as a new version.

## Signing details

**Entitlements: none.** The app is signed with the hardened runtime and no entitlements, which is the minimum it needs:

- **Outgoing network and the loopback OIDC listener.** The `com.apple.security.network.client` and `network.server` entitlements apply only to App Sandbox apps. Commons is not sandboxed, so under the hardened runtime it connects to servers and binds `127.0.0.1:0` for the OIDC return without any entitlement.
- **WKWebView.** WebKit runs JavaScript and WebAssembly JIT in its own WebContent process, which Apple signs with its own entitlements. The app process never maps writable executable memory, so it needs neither `cs.allow-jit` nor `cs.allow-unsigned-executable-memory`.
- **Keychain, notifications and file dialogs.** These need no entitlement outside the sandbox.

If a later feature needs one, add an `Entitlements.plist` through `bundle.macOS.entitlements`. Microphone and camera for huddles are examples: they need `com.apple.security.device.audio-input` and `com.apple.security.device.camera` under the hardened runtime, plus usage strings.

**The keychain across updates.** The keychain stores session tokens with an access list that names the app by its designated requirement: the bundle identifier `org.commons.desktop`, signed by your team's Developer ID. Every release is signed with the same identity, so every release satisfies the same requirement, and a token saved by one version is read by the next without a password prompt. Debug builds are ad-hoc signed, and their requirement changes on every build. That is why they keep tokens in a file instead. To compare two builds:

```sh
codesign -d -r- old/Commons.app
codesign -d -r- new/Commons.app   # identical: identifier "org.commons.desktop" and anchor apple generic and … leaf[subject.OU] = TEAMID
```

**Why the release notarizes rather than Tauri.** `tauri build` notarizes only with an Apple ID password or an API key in environment variables, and it has no notarytool keychain profile option. The release therefore unsets those variables, so Tauri only signs, and then runs `xcrun notarytool submit --keychain-profile commons-notary` itself. If notarization is rejected, the release prints Apple's log and stops.

## Test builds

`cargo xtask release --config <overlay.json>` merges a Tauri config overlay into the build, for example to point the updater at a local server. Set `plugins.updater.endpoints`, plus `dangerousInsecureTransportProtocol` for plain http, and optionally a throwaway `pubkey` together with `TAURI_SIGNING_PRIVATE_KEY_PATH` for its key. The release verifies the update signature against the overlay's key. Never publish a build made with an overlay.
