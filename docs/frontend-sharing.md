# Shared frontend and downstream adapters

`frontend/src/shared/` is the common UI implementation: terminals,
connections, account settings, navigation, browser requests and PWA behavior.
Shared styles and assets remain in `frontend/`. `src/main.rs` mounts that
implementation with an edition adapter at `src/edition.rs`.

The adapter shipped here is passkey-only. It has no external-provider routes,
provider discovery or redirect code. Downstream applications can keep their
own adapter while consuming the same shared files and assets, avoiding a
fork of the main app. The adapter supplies presentation/step-up hooks; it
never replaces server-side authorization.

The internal adapter interface supplies state initialization, post-login
handling, a terminal-access decision from the current account response,
optional sign-in/account views, passkey-management availability, an account
summary suffix and reauthentication. Both adapters compile against their
own backend's protocol crate. Changes to this interface or protocol types
must be tested in consuming deployments before rollout.

For local side-by-side development, a consumer can link its shared module
directory and assets to this checkout, while keeping its own `edition.rs`
and build output. For CI, check out an exact public commit and use that
same commit for testing and deployment. Changes to public files then flow
into both builds without copying private authentication code here.

Deployment automation belongs in the consumer: it can poll public commits
and run its own tests and deployment. The public project requires no private
deployment token or credentials. Each self-hosted installation controls
whether and when to take updates; the normal self-hosting deployment
procedure remains available.
