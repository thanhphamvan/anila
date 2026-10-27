# v0.0.2: accounts, many machines, sharing

Status: agreed design, source of truth for the 0.0.2 tasks. The wire
protocol between browser, room and service (`proto/src/control.rs`,
`data.rs`) does not change; `PROTO` stays 1. What changes is who may reach
which room, and the HTTP API around it.

## What the user gets

- **Accounts.** One deployment serves many people. Each account has its own
  passkeys, machines and notification subscriptions, and never sees another
  account's machines unless they were shared with it.
- **Many machines per account.** The web app opens on a machine list with
  online status; pick one to see its tmux sessions.
- **Pairing from the web app.** "Add machine" shows a one-time code; on the
  new machine `anila-service pair <code>` registers its public key with the
  account. No worker redeploy, no `HOST_KEYS` edit.
- **Sharing.** A machine's owner creates a one-time share code; another
  signed-in account redeems it and becomes a *member*. Members get full
  terminal access (it is a shell on the owner's machine, say so in the UI)
  but cannot rename, share or remove the machine. The owner can remove a
  member at any time, which closes the member's open terminals at once.
- **Invites.** New accounts need an invite code from an admin. The account
  that exists after the upgrade (today's passkeys) becomes the first admin.
- **Account page.** Passkeys (list, delete, add another device with a
  device code), notifications, sign out; for admins, invites and accounts.

### Why the web app makes the pairing code

The code travels from the signed-in user to the machine. Whoever holds a
pairing code can attach *a machine of their choice* to *the code creator's
account*. If the machine showed a code for the user to type into the web
app instead, anyone who saw that code could claim the machine into their
own account and get a shell on it. With this direction a leaked code at
worst adds the thief's machine to your list.

## Identities

| Thing | Id | Notes |
| --- | --- | --- |
| Account | 16 random bytes, base64url (22 chars) | Also the WebAuthn user handle of its passkeys. The upgraded account keeps today's `meta.user_id`, so existing passkeys keep working. |
| Machine (host) | `HostId` (`^[a-z0-9-]{1,32}$`) | New: `h` + 15 random chars from `a-z2-7` (`auth_core::host_id_from_bytes`). Upgraded: the old host name from `HOST_KEYS`, so running services need no change. The id names the room (`idFromName(id)`) and is what the service puts in `Term-Host` and signs. |
| Machine name | `HostId` syntax, unique per owner | Display name, e.g. `mac-mini`; renamable. Never used for routing. |
| Account name | 1–40 chars, trimmed, no control chars | Display only, not unique. |

## Codes

One table, four kinds. Format and normalization are today's enroll code
(`XXXX-XXXX`, Crockford-style alphabet, 40 bits); only the SHA-256 of the
normalized code is stored. Every code is single-use.

| Kind | Created by | Lifetime | Redeemed by | Effect |
| --- | --- | --- | --- | --- |
| `device` | `anila-service enroll` (signed; for the machine's owner) or a signed-in user with step-up | 10 min | the sign-in page | Adds a passkey to that account. A `device` code with no account (only possible while there are no accounts) bootstraps the first admin. |
| `invite` | admin, step-up | 7 days | the sign-in page | Creates a new (non-admin) account with a passkey. |
| `pair` | signed-in user, step-up | 10 min | `anila-service pair` | Creates a machine owned by the code's account. |
| `share` | the machine's owner, step-up | 24 h | a signed-in account | Makes that account a member of the machine. |

Brute force: every failed code lookup counts toward a per-minute total in
the auth store; above `CODE_FAILURES_PER_MINUTE` all code redemptions in
that minute fail with 429 `limit`.

## Limits (`proto/src/limits.rs`)

`MAX_ACCOUNTS` 200, `MAX_HOSTS_PER_ACCOUNT` 20 (owned), `MAX_MEMBERS_PER_HOST`
20, `MAX_PASSKEYS_PER_ACCOUNT` 20, `PAIR_CODE_SECS` 600, `SHARE_CODE_SECS`
86 400, `INVITE_CODE_SECS` 604 800, `DEVICE_CODE_SECS` 600 (=
`ENROLL_CODE_SECS`), `CODE_FAILURES_PER_MINUTE` 30, `ACCOUNT_NAME_MAX` 40.

## Auth store schema (version 2)

```sql
accounts    (id TEXT PRIMARY KEY, name TEXT NOT NULL, is_admin INTEGER NOT NULL,
             created_at INTEGER NOT NULL)
credentials (+ account_id TEXT)                -- existing table, new column
sessions    (unchanged; account = the credential's account)
hosts       (id TEXT PRIMARY KEY, owner_id TEXT, name TEXT NOT NULL,
             public_key TEXT NOT NULL, created_at INTEGER NOT NULL)
             -- owner_id NULL = imported from HOST_KEYS before any account existed
members     (host_id TEXT NOT NULL, account_id TEXT NOT NULL, created_at INTEGER NOT NULL,
             PRIMARY KEY (host_id, account_id))
codes       (code_hash TEXT PRIMARY KEY, kind TEXT NOT NULL, account_id TEXT,
             host_id TEXT, expires_at INTEGER NOT NULL, created_at INTEGER NOT NULL)
flows       (+ user_id TEXT, + code_kind TEXT)  -- existing table, new columns
push_subs   (+ account_id TEXT)                 -- existing table, new column
code_failures (minute INTEGER PRIMARY KEY, count INTEGER NOT NULL)
meta        ('schema' = '2', 'host_keys' = sha256 of the last imported HOST_KEYS)
```

`enroll_codes` is dropped (outstanding enroll codes are lost; they last 10
minutes anyway).

### Upgrade (runs once, idempotent, before any request is handled)

1. Create the new tables and columns.
2. If `accounts` is empty and `credentials` is not: create the admin account
   with id = `meta.user_id`, name `"Owner"`, `is_admin = 1`; set
   `account_id` on every credential and push subscription.
3. Set `meta.schema = 2`.

### `HOST_KEYS` import (whenever its hash differs from `meta.host_keys`)

For each `name → key`: insert a host with `id = name`, `name = name`, that
key, and `owner_id` = the oldest admin account (NULL if there is none); if
the host exists, update its key. Hosts removed from `HOST_KEYS` stay.
`HOST_KEYS` is now only for bootstrapping a fresh deployment; pairing is the
normal way to add machines. `DEFAULT_HOST` is read only in `DEV_INSECURE`
mode.

### Bootstrap (fresh deployment)

`anila-service enroll` on a `HOST_KEYS` machine whose host has no owner
creates a `device` code with no account. Redeeming it (only while
`accounts` is empty) creates the first admin account and gives it every
ownerless host. Once an account exists, ownerless hosts no longer occur
(the import assigns the oldest admin).

## Access rules

- `access(account, host)` = `owner` if `hosts.owner_id = account`, `member`
  if a `members` row exists, else none.
- Owner only: rename, share codes, list and remove members, remove machine.
- Member: use the terminal, receive the machine's notifications, leave.
- Admin: create invites, list accounts, delete an account (not their own).
  Admin rights grant no access to other accounts' machines.
- Step-up (signed in within `STEP_UP_SECS`) is required to create any code,
  delete a passkey, remove a machine, delete an account.
- A machine that does not exist and a machine the account cannot access
  answer the same: 404 `not_found`. A member asking for an owner-only
  action gets 403 `forbidden`; a non-admin calling an admin route too.
- Successful calls answer 200 (`{}` when there is nothing to return),
  except the existing register (201) and login/logout (204) replies.

## Browser HTTP API (worker)

Same conventions as v0.0.1: JSON bodies, CSRF check on every non-GET,
errors `{"error": {"code", "msg"}}`, the session in the `__Host-term`
cookie. Shapes are the structs in `proto/src/api.rs`.

| Method and path | Who | Body → reply |
| --- | --- | --- |
| `GET /api/me` | signed in | → `Me {account: {id, name, admin}, auth_at, expires_at}` |
| `GET /api/hosts` | signed in | → `[HostView {id, name, role, owner_name, online, since}]`, owned first then shared, each by name. `online`/`since` come from the room's `/internal/status`. |
| `PATCH /api/hosts/{id}` | owner | `{name}` → `HostView`; 409 `exists` if the owner already has that name |
| `DELETE /api/hosts/{id}` | owner, step-up | → `{}`; deletes members and codes for it; the room closes the service (4403 "machine removed") and every browser (4403) |
| `GET /api/hosts/{id}/members` | owner | → `[Member {account_id, name, since}]` |
| `DELETE /api/hosts/{id}/members/{account_id}` | owner, or the member themself (`me` works as the id) | → `{}`; the room closes that account's browsers (4403) |
| `POST /api/hosts/{id}/share-codes` | owner, step-up | `{}` → `CodeIssued {code, expires_at}` |
| `POST /api/share/redeem` | signed in | `{code}` → `HostView`; 409 `exists` if already owner or member; 409 `limit` at `MAX_MEMBERS_PER_HOST` |
| `POST /api/pair-codes` | step-up | `{}` → `CodeIssued`; 409 `limit` at `MAX_HOSTS_PER_ACCOUNT` |
| `POST /api/device-codes` | step-up | `{}` → `CodeIssued` |
| `POST /api/codes/check` | anyone | `{code}` → `{kind: "device" \| "invite"}`; pair and share codes answer 404 here |
| `POST /api/passkeys/register/options` | code, or signed in with step-up | `{code?}` → unchanged shape. `device` code: that account's passkeys are excluded and its id is the user handle; `invite` code (or the bootstrap `device` code): a fresh random user handle, kept in the flow. |
| `POST /api/passkeys/register/verify` | | `{flow, label, credential, account_name?}` → `{id}` + cookie. `account_name` is required for an invite (400 `invalid_name` if missing or bad), optional for the bootstrap code (default `"Owner"`), ignored otherwise. |
| `POST /api/passkeys/login/*` | | unchanged; the credential determines the account |
| `GET /api/passkeys`, `DELETE /api/passkeys/{id}` | signed in | scoped to the account; "last passkey" means the account's last |
| `POST /api/admin/invites` | admin, step-up | `{}` → `CodeIssued` |
| `GET /api/admin/accounts` | admin | → `[AccountView {id, name, admin, created_at, hosts, passkeys}]` |
| `DELETE /api/admin/accounts/{id}` | admin, step-up, not self (409 `limit`) | → `{}`; deletes its passkeys, sessions, push subscriptions, memberships, and owned machines (as `DELETE /api/hosts/{id}`) |
| push, logout | | unchanged, scoped to the account |

## Websockets

- `GET /ws/term?host=<id>`: the worker checks the session and
  `access(account, host)` in one auth store call (`/access`); 404 if none.
  Forwarded headers gain `Term-Account` (account id) and `Term-Host-Name`.
  The room stores the account on the browser socket and sends the machine
  *name* in `welcome.host`. Without `?host=`, the worker uses the account's
  only machine if it has exactly one (old cached web apps keep working),
  else 400 `bad_frame`.
- `GET /ws/agent?host=<id>`: the worker loads the machine's public key from
  the auth store (`/hosts/key`); unknown id → 404 before upgrading. The key
  is forwarded in `Term-Host-Key`; the room verifies the `auth` signature
  against it instead of `HOST_KEYS`. As before the worker strips every
  incoming `Term-*` header, so the room can trust these.
- Room `POST /internal/revoke` accepts `{"sid_hash"}`, `{"all": true}`,
  `{"account"}` (close that account's browsers, 4403 "access removed"), or
  `{"host_removed": true}` (close the service and every browser with 4403
  "machine removed").
- Room `POST /internal/status` → `AgentStatus` (online, since).
- Revocations now go only to the rooms the affected account can access,
  not to every room.

## Signed service calls

Headers and signing input are unchanged (`proto/src/sig.rs`): `Term-Host` is
the machine id and picks the key, now from the auth store instead of
`HOST_KEYS`.

| Call | Effect |
| --- | --- |
| `POST /api/agent/pair` | Special: `Term-Host: pair`; the body `{code, name, public_key}` carries the key and the signature must verify with it. Redeems a `pair` code → `Paired {host, name, account_name}`. `name` is made unique within the owner's machines (`mac-mini`, `mac-mini-2`, …). 409 `exists` if that public key is already paired. |
| `POST /api/agent/enroll-code` | A `device` code for the machine's owner (or the bootstrap code, see above). |
| `POST /api/agent/revoke-all` | Signs out every session of the machine's owner account and closes every browser in that machine's room. |
| `POST /api/agent/notify` | Delivers to the push subscriptions of the owner and every member. The payload gains `host` (id) and `host_name`; clicking it opens `/?host=<id>&session=<name>`. |

## Service

- `anila-service pair <CODE> [--name NAME] [--worker URL]`: creates the key
  if missing, calls `/api/agent/pair`, then writes `host = "<id>"` (and
  `worker` if given) into `service.toml`, creating it if needed and
  preserving every other line. Default name: the machine's hostname,
  lowercased, with every run of characters outside `[a-z0-9-]` replaced by
  `-`, trimmed of `-`, cut to 32 (`mac` if that leaves nothing).
- `host` in `service.toml` is the machine id. Legacy names keep working.
- A 404 on the agent socket means "not paired": log
  `this machine is not paired (run anila-service pair)` and retry slowly.
- Log every close from the room with its code and reason (`4401 bad
  signature`, `4403 machine removed`), so a refused or removed machine is
  visible in the service log.

## Web app

Screens: sign-in → **Machines** (default; opens the only machine directly
when there is exactly one, else reopens the last machine used if it is
still listed) → **Sessions** (one machine) → **Terminal**. Plus
**Account** and, for owners, **Machine settings**.

Sessions and Terminal share one row at the top instead of headers: a home
button (to Machines), a tab per session opened on this device (across
machines, kept in localStorage; a tab names its machine when the tabs span
several), "+" for the current machine's sessions, and the status (a dot
while live, words otherwise). A tab on another machine reconnects there.
Closing a tab only detaches; tabs of ended sessions and removed machines
go away.

- Sign-in: "Sign in with passkey" and "Have a code?" (one field; after
  `/api/codes/check`, ask for a device label, plus an account name for an
  invite).
- Machines: name, online dot, "shared by <owner>" for members; buttons "Add
  machine" (shows the code, the exact `anila-service pair <code>` command
  with the worker URL, and waits until the machine appears) and "Join a
  shared machine" (code field).
- Machine settings (owner): rename, "Share" (code + who it is for), members
  with "Remove", "Remove machine". Member: "Leave".
- Account: name, passkeys with delete, "Add another device" (device code),
  notifications toggle, sign out; admins also see "Invite someone" and the
  accounts list with delete.
- A 401 `step_up` reply triggers a passkey prompt, then the request is
  retried once.
- One terminal connection at a time; switching machines closes the old
  socket (its sessions keep running and resume within the grace period).

## Compatibility

- v0.0.1 services connect unchanged (their host name is the machine id).
- v0.0.1 passkeys sign in to the upgraded admin account.
- A cached v0.0.1 web app keeps working for an account with one machine.
- Rolling back the worker to v0.0.1 after the upgrade is not supported
  (the schema moved on); take a store export first if that matters.

## Tests that define done

- `worker/tests/auth_core.rs`: ids, names, code kinds and lifetimes, access
  decisions, unique machine names (native, no wrangler).
- `service/tests/pair.rs`: config rewrite and default names.
- `e2e/tests/tenancy.rs`: bootstrap admin, pairing a second machine,
  invites, isolation (another account cannot list, open, share or enroll
  for a machine), sharing and live revocation, machine removal, push fan-out
  to owner and members, admin account deletion.
- `e2e/tests/upgrade.rs`: a store written by the v0.0.1 worker, then the
  v0.0.2 worker on the same state: the old passkey signs in as admin, the
  old machine is listed and its v0.0.1 service connects.
- The existing functional, chaos, browser and push suites keep passing.
