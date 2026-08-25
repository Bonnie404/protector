# Protector

A GNOME panel widget that counts down to the end of the Google Calendar
block you selected. It sits in the top bar as a tray icon with a live
countdown label (`42:17 · Design review`), lets you pick which of today's
events it should track from a menu, and pings you with a notification when
time is up.

*(No screenshot yet — this is a hand-written `org.kde.StatusNotifierItem`
tray icon, best captured from a real GNOME top bar rather than described.)*

## What it does

- Shows a ticking countdown for whichever calendar block you have selected,
  right in the panel: `1:23:45 · Design review` when there's over an hour
  left, `42:17 · Design review` under an hour.
- Left-click opens a menu listing today's events — pick one to track it.
- Goes into overtime instead of disappearing when a block runs long:
  `⚠ +04:31 · Design review`, with the icon switching to a red hourglass.
- Sends a low-urgency heads-up 5 minutes before a block ends, and a second
  notification right at the end with one button per upcoming task so you can
  jump straight to the next thing.
- Syncs against the real Google Calendar API — no stale cached feed.
- Read-only: Protector cannot create, edit, or delete anything on your
  calendar. It only ever asks for `calendar.events.readonly`.

## 1. Set up a Google Cloud OAuth client

Protector talks to the Google Calendar API as *your own* application, which
means it needs an OAuth client id and secret before it can connect. This
takes about five minutes and costs nothing.

1. Go to the [Google Cloud Console](https://console.cloud.google.com/) and
   either create a new project or pick an existing one.
2. Under **APIs & Services → Library**, search for **Google Calendar API**
   and enable it for that project.
3. Under **APIs & Services → OAuth consent screen**, configure a consent
   screen:
   - User type: **External** is fine — you do not need a Workspace account.
   - Publishing status: leave it in **Testing**. There is no need to submit
     it for verification; a testing-mode app works indefinitely for its own
     test users.
   - Under **Test users**, add your own Google account's email address. This
     is the account whose calendar Protector will read.
4. Under **APIs & Services → Credentials**, click **Create credentials →
   OAuth client ID**.
   - Application type: **Desktop app**. (Not "Web application" — Protector
     has no web server, and a Desktop app client is the one Google issues a
     refresh token to without requiring a fixed HTTPS redirect URI.)
   - Give it any name you like and create it.
5. Copy the **Client ID** and **Client secret** it shows you — you'll paste
   them into `config.toml` in the next step.

## 2. Install

```sh
git clone <this repository>
cd protector
./install.sh
```

This builds a release binary and installs:

| What | Where |
| --- | --- |
| The `protector` binary | `~/.local/bin/protector` |
| The panel and overtime icons | `~/.local/share/icons/hicolor/scalable/apps/` |
| The systemd user unit | `~/.config/systemd/user/protector.service` |

It also refreshes the icon cache and runs `systemctl --user daemon-reload`.
**It does not start or enable the service** — see the last step below.

Make sure `~/.local/bin` is on your `PATH` (most GNOME desktops already put
it there for you).

## 3. Configure

The first time you run `protector` — including the first `protector login`
below — it writes a commented template to
`~/.config/protector/config.toml` if one doesn't exist yet. Open it and fill
in the two values from step 1:

```toml
client_id     = "your-client-id.apps.googleusercontent.com"
client_secret = "your-client-secret"
calendar_id   = "primary"
warn_before_minutes = 5
```

`calendar_id` defaults to `primary` (your main calendar); point it at
another calendar's id if you want Protector tracking a different one.

## 4. Connect your account

```sh
protector login
```

This opens your browser to Google's consent screen (read-only access to
calendar events). Once you approve it, the refresh token is stored — in the
**GNOME keyring** when a Secret Service is available, or otherwise in a
`0600` file at `~/.local/state/protector/token.json`. Either way, the token
never appears in a log, in `protector status`, or anywhere else.

Run `protector status` any time to see whether you're connected, which
store the token lives in, and when the last sync happened.

## 5. Run it

For a one-off check:

```sh
protector run
```

Or, once you've decided you want it running every login (this is a
deliberate choice `install.sh` leaves to you — see below):

```sh
systemctl --user enable --now protector.service
```

A second `protector` (run directly or as the service) refuses to start if
one is already running — it will print a clear message and exit rather than
create a second tray icon.

## Using it

- **Left-click** the tray icon to open the menu of today's calendar blocks.
  Because Protector deliberately doesn't implement the `Activate` D-Bus
  method (there is nothing sensible for a plain single left-click to
  *activate*), the very first click after the panel host starts may wait out
  the system's normal double-click interval before the menu appears — every
  click after that opens the menu immediately, once the host has learned
  activation isn't supported.
- **Middle-click** the tray icon to force an immediate sync, instead of
  waiting for the next automatic one.
- Selecting an item in the menu switches the countdown to that block right
  away.
- **`Refresh now`** in the menu does the same as a middle-click.
- **`Connect Google Calendar…`** / **`Disconnect account`** in the menu do
  the same thing as `protector login` / `protector logout`, without leaving
  the panel.

### What the label shows

| Situation | Label |
| --- | --- |
| More than an hour left | `1:23:45 · Design review` |
| Less than an hour left | `42:17 · Design review` |
| Overtime | `⚠ +04:31 · Design review` (icon turns red) |
| Nothing selected | `Pick a task` |
| Not connected | `Connect calendar` |

## Logs

```sh
journalctl --user -u protector -f
```

only shows anything once the systemd service is running — a `protector run`
started directly from a terminal prints straight to that terminal instead.

## Disconnecting

```sh
protector logout
```

Clears the refresh token from every place it could be — the keyring and the
file fallback — and returns the panel to `Connect calendar`. Google still
lists Protector under your account's connected apps until you remove it
yourself at <https://myaccount.google.com/permissions>; `logout` only
revokes what Protector itself holds locally, it does not call Google's
revoke endpoint.

## Uninstalling

```sh
systemctl --user disable --now protector.service   # if you had enabled it
rm ~/.local/bin/protector
rm ~/.local/share/icons/hicolor/scalable/apps/protector.svg
rm ~/.local/share/icons/hicolor/scalable/apps/protector-attention.svg
rm ~/.config/systemd/user/protector.service
systemctl --user daemon-reload
protector logout   # before removing the binary, to clear the stored token
```

`~/.config/protector/config.toml` and `~/.local/state/protector/` are left
alone — remove them by hand if you want a completely clean slate.
