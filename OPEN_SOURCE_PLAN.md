# Open-source preparation status

The public edition lives in a new local repository at `anila-open-source`,
created from source commit `9d47c6b` without private Git history or ignored
operational files. Van-Thanh Pham authorized MIT licensing on 2026-10-27.
The original repository is unchanged.

## Implemented

- Added the MIT License with Van-Thanh Pham's copyright and shared Cargo
  license/author metadata. Added contribution and private security-reporting
  instructions using the supplied email addresses.
- Removed IdP API shapes, limits, token verification, internal/public routes,
  account linking and login screens, reauthentication branches, environment
  configuration and provider-specific tests. Used the last passkey-only
  implementation as the basis; terminal/replay code and protocol stay intact.
- The edition targets fresh installations. Auth initialization refuses schema
  3 and unknown newer stores before rewriting authentication data. No IdP
  identity or machine-ownership migration is offered. New integration tests
  cover removed routes, persisted passkeys and rejection of a newer store.
- Removed production Worker and upstream repository defaults from installers.
  A new host requires the user's Worker URL; release downloads require an
  explicit repository. Public Linux release downloads need no GitHub token.
  Missing inputs fail before machine mutation. Reinstallation preserves
  existing pairing/configuration.
- Local and Actions deployments share origin/RP validation and custom-domain
  route generation. Pinned Wrangler and worker-build are used in the guide
  and deployment tools. Added configuration/installer regression tests and
  CI integration coverage.
- Replaced the user guide and added `docs/self-hosting.md`: first-host
  enrollment, Cloudflare configuration, Linux/systemd, macOS/launchd,
  additional hosts, invitations, machine trust, optional Access/push and
  recovery. No private Git tags or private upstream releases are required.
- Added an inventory of locked dependency license metadata and upstream
  supplements where crate archives omit notices. Release archives collect
  license texts for their target's service dependency graph and include the
  project license and third-party notices. Vendored xterm remains unchanged.
- Delegated benchmark definition to the requested Astra model; its source
  review and reproducible specification are in
  `bench/OPEN_SOURCE_BENCHMARK_PLAN.md`. This work defines measurements and
  does not fabricate new performance results.

## Review findings and remaining publication work

1. The existing protocol/sans-IO boundary, shared vectors and broad test
   harness provide a useful base. Initial source review is not an independent
   security audit. Review prospective tracked files for credentials and
   identifying data before any public push; fresh history is not a secret scan.
2. Source uses Rust libraries under their own licenses. The inventory lists
   ten crates without available full license text in their downloaded source
   or a pinned local supplement. Their metadata is included; review upstream
   notices before redistributing binaries containing them (notably frontend
   wasm). This does not change the MIT license on anila's own source.
3. Select the public GitHub owner/name. Installation tools accept
   `ANILA_REPO=owner/repository`; Actions use their own repository. A remote
   is deliberately unset until the destination is selected. No publication,
   production deployment or release tag has been made.
4. Validate a clean deployment in an independent Cloudflare account and run
   the macOS/launchd and physical-phone instructions on those platforms.
   Local simulation does not establish live Cloudflare readiness or cost.
5. Implement the benchmark plan's release-service selection and exact-byte
   validation before using its proposed performance claims. Existing network
   runs start a debug service and burst completion can observe a tmux
   snapshot; browser decode measures length, not full byte equality.

See `VALIDATION.md` for commands run and actual results from this change.
