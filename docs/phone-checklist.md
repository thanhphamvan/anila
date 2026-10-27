# Real-phone checklist

The automated suites cover everything a desktop machine can simulate:
link hangs, cuts, latency, bandwidth limits, a sleeping or crashed host,
and a real Chromium browser with a virtual passkey. What they cannot cover
is a real phone radio. Run this list on an iPhone (or Android) after
deploying, once per release.

`e2e/tests/production.rs` already automates an emulated version of items
1, 2, 6 and part of 10 against the live site (an iPhone-sized touch device in
headless Chromium on throttled 4G, with a 10 s dropout), and
`e2e/tests/production_push.rs` proves real push delivery to a desktop
browser through a real push service. Neither can cover Face ID itself, a real
radio handing over between Wi-Fi and 4G, iOS suspending a locked or
backgrounded app, or Apple's push service delivering to an installed iPhone
app, so the rows below still need a real phone.

Setup: open your deployed URL in Safari, sign in with Face ID, then
Share → Add to Home Screen, and open anila from the Home Screen icon. Turn
on Notifications in the session list. Open a session running Claude Code
or `top`.

| # | Do this | Expect |
|---|---|---|
| 1 | Sign in with Face ID; cancel once, then retry | Cancel shows a plain message; retry signs in |
| 2 | Type in the terminal with the soft keyboard; use the key bar's Esc, Ctrl, arrows | Keys reach tmux; Ctrl then a letter sends a control key |
| 3 | Switch from Wi-Fi to 4G while output is streaming | Pill shows "Reconnecting…" briefly, then "Live"; no missing lines |
| 4 | Lock the phone for 5 minutes, unlock | The app reconnects within a few seconds and shows what happened meanwhile |
| 5 | Lock the phone for 15 minutes (past the 10-minute grace), unlock | The screen redraws once (reset) and works; scroll back in tmux for older output |
| 6 | Ride an elevator or walk out of coverage for 30 s, then back | Input pauses ("Input paused while reconnecting"), then resumes; nothing typed twice |
| 7 | Close the app completely, run `anila-service notify --title Test --body Hi` on the host | A notification arrives; tapping it opens the app |
| 8 | Finish a Claude Code turn with the hook installed | "Claude finished" arrives; tapping opens that session |
| 9 | Put the host Mac to sleep | Pill shows "Host offline since HH:MM" within about 30 s; waking the Mac brings it back |
| 10 | Airplane mode, open the app from the Home Screen | The app shell opens from cache and shows it is reconnecting |

Write down anything that misbehaves with the time, the network, and what
the pill said; the service log (`/tmp/anila-service.log` under launchd)
and `npx wrangler tail` show the matching server side.
