# Commons

**The open-source team chat that belongs to everyone.**

Commons is a free, open-source, community-driven alternative to Slack and Microsoft Teams: a shared space where teams talk, plan, and build together, without lock-in, per-seat pricing, or someone else owning your conversations.

> **Status:** Early stage. Commons is just getting started, so expect rough edges and big changes. Ideas and contributions are very welcome.

---

## Why Commons?

A *commons* is a shared resource that a community looks after together. Team communication should work the same way: open, owned by the people who use it, and not rented back to them.

- **Open source, forever.** The code is public and the project is non-commercial.
- **Yours to run.** Self-host it on your own infrastructure and keep your data where you want it.
- **Built for teams.** Channels, threads, and direct messages that keep conversations organized, not buried.
- **Community-shaped.** The roadmap is decided in the open, by the people who use it.

## Planned features

- [ ] Workspaces with public and private channels
- [ ] Threaded conversations
- [ ] Direct and group messages
- [ ] Mentions, reactions, and notifications
- [ ] File sharing
- [ ] Search across messages and files
- [ ] Voice and video huddles
- [ ] Integrations and bots via an open API
- [ ] Self-hosting with a single command

## Getting started

Commons isn't ready to run yet. Setup instructions will appear here once the first version is available.

For development (Rust via rustup, the `wasm32-unknown-unknown` target, `trunk`, `tauri-cli`, and Docker):

- `cargo xtask dev` starts Postgres in Docker and a local `commonsd` on `http://127.0.0.1:8080`, then builds and runs the desktop app.
- `cargo xtask app` builds the desktop app once and runs it. Extra arguments and environment pass through to the app.

Both build once with `cargo tauri build --debug --no-bundle` and run `target/debug/commons-desktop` directly; rerun them to pick up changes. Avoid `cargo tauri dev`: it rebuilds on every workspace change, and on macOS a Ctrl+C can leave its cleanup script spinning. Debug builds keep session tokens in `secrets.json` in the app's data directory rather than the keychain, because each rebuild changes the binary's signature and the keychain would ask for a password on every launch. Set `COMMONS_SECRETS=keychain` or `file` to choose either store.

To build a signed, notarized Mac release with its update files, see [Releasing the Mac app](docs/release.md) (`cargo xtask release`).

## Contributing

Commons is built in the open, and every kind of help counts: code, design, docs, testing, and ideas.

1. Open an issue to report a bug or suggest a feature.
2. Fork the repo and create a branch for your change.
3. Open a pull request describing what you changed and why.

## License

To be decided. An OSI-approved open-source license will be added before the first release.
