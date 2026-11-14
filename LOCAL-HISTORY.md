# Restore a local secret version

The dashboard can show earlier versions stored on this device. You need an
initialised project, a local identity and an unlocked store, but no server login
or network connection. See [opening the dashboard](README.md#interactive-dashboard).

Select the right environment, choose a secret such as `DEMO_API_KEY`, then press
`h` to open its history. Only versions held on this device appear. For versions
on the server, use the separate [server history guide](README.md#restore-an-earlier-secret-version).

## Look through the versions

Values start hidden. In the history dialogue:

- Use the arrow keys or `j` and `k` to choose a version.
- Press `r` or Space to show or hide values.
- Press `c` to copy the selected value. This needs a readable UTF-8 value and a
  working clipboard.
- Press `Esc` or `q` to close history without restoring anything.

## Restore a version

Press `Enter` or `b` to restore the selected version immediately. There is no
confirmation prompt. These keys change the secret, rather than just previewing it.

Sotto saves the chosen value as a new local version and keeps the older history.
On success, the dialogue closes and a status message shows the new version number.
An unreadable version cannot be restored; retrying does not guarantee recovery.

Restoring does not push the change to the server. If you want to sync it, quit the
dashboard and push using the same environment. A dashboard environment switch
does not change the project's default, so pass the environment explicitly if needed.
See [environment selection](README.md#environments) and
[logging in and pushing](README.md#quick-start).
