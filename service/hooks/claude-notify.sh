#!/bin/sh
# Claude Code hook: push "Claude finished" / "Claude needs you" to your devices.
# Set ANILA_SERVICE to the binary path if it is not on the hook's PATH.
# Keep hook completion independent of notification delivery.
payload=$(cat)
event=$(printf '%s\n' "$payload" | sed -n 's/.*"hook_event_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
message=$(printf '%s\n' "$payload" | sed -nE 's/.*"message"[[:space:]]*:[[:space:]]*"(([^"\\]|\\.)*)".*/\1/p' | sed 's/\\"/"/g; s/\\\\/\\/g; s/\\n/ /g; s/\\r/ /g; s/\\t/ /g; s@\\/@/@g')
case "$event" in
    Stop) title='Claude finished' ;;
    Notification) title='Claude needs you' ;;
    *) exit 0 ;;
esac
body=${message:-Ready for your next prompt}
(
    "${ANILA_SERVICE:-anila-service}" notify --title "$title" --body "$body" &
    notify_pid=$!
    (
        sleep 5
        kill -KILL "$notify_pid" 2>/dev/null
    ) &
    guard_pid=$!
    wait "$notify_pid"
    kill "$guard_pid" 2>/dev/null
    wait "$guard_pid" 2>/dev/null
) </dev/null >/dev/null 2>&1 &
exit 0
