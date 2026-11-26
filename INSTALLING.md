# Choose a CLI version and install folder

The [shell installer](install.sh) and [PowerShell installer](install.ps1) accept
two environment variables:

- `SOTTO_VERSION`: the exact release tag, including the `v` prefix. Leave it unset
  or empty to install the latest published release. Choose a tag from the
  [releases page](https://github.com/getsotto/sotto/releases).
- `SOTTO_INSTALL_DIR`: where to put the executable. If unset or empty, this is
  `~/.local/bin` on macOS/Linux or `%LOCALAPPDATA%\sotto\bin` on Windows.

The examples below use `v0.9.0` as an example tag, not a claim about the latest
release. Installing into a folder that already contains Sotto replaces that
folder's executable. Other installations can remain on your machine.

## macOS and Linux

Pass the variables to `sh`, on the right of the pipe:

```sh
curl -fsSL https://raw.githubusercontent.com/getsotto/sotto/main/install.sh |
  SOTTO_VERSION="v0.9.0" SOTTO_INSTALL_DIR="$HOME/Sotto CLI/bin" sh
"$HOME/Sotto CLI/bin/sotto" --version
command -v sotto
```

These assignments apply only to the installer process, so there is nothing to
clear afterwards. Assigning them to `curl` instead would not pass them to `sh`.

## Windows PowerShell

Set the variables before running the installer. This example saves any existing
values and restores them afterwards, even if installation fails:

```powershell
$previousVersion = $env:SOTTO_VERSION
$previousInstallDir = $env:SOTTO_INSTALL_DIR
try {
    $env:SOTTO_VERSION = 'v0.9.0'
    $env:SOTTO_INSTALL_DIR = "$env:LOCALAPPDATA\Sotto CLI\bin"
    irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
    & "$env:SOTTO_INSTALL_DIR\sotto.exe" --version
    Get-Command sotto -ErrorAction SilentlyContinue
} finally {
    $env:SOTTO_VERSION = $previousVersion
    $env:SOTTO_INSTALL_DIR = $previousInstallDir
}
```

If either variable was unset before the example, restoring its null value removes
the temporary override. Avoid clearing a variable blindly if you had already set it.

## Check which installation you use

The explicit-path version commands above check the binary in your chosen folder.
`command -v sotto` and `Get-Command sotto` show what a plain `sotto` command finds
on `PATH`. That may still be an older installation, or no command at all.

The installer does not change `PATH`. To prefer the example folder in your current
terminal session, use the matching command below:

```sh
export PATH="$HOME/Sotto CLI/bin:$PATH"
```

```powershell
$env:Path = "$env:LOCALAPPDATA\Sotto CLI\bin;$env:Path"
```

Run the command-resolution check again. For future terminals, add the folder to
your shell's startup configuration or Windows user `PATH` settings.

`SOTTO_INSTALL_DIR` controls only the executable's location. `SOTTO_DATA_DIR`
controls local application data; neither setting is a complete account migration.
Keep your Emergency Kit safe when moving between installations or devices.

The normal checksum and optional Sigstore checks still apply. See the
[README installation section](README.md#install) for verification and Homebrew
guidance, and [SECURITY.md](SECURITY.md) for manual verification steps.

## Remove a script-installed copy

The installers copy one executable, `sotto` (or `sotto.exe` on Windows), into the
install folder. To remove that copy, delete only that file. For Homebrew, use the formula
instructions in the [README](README.md#homebrew-preview) instead.

First look at what is in the folder and what a plain `sotto` currently resolves to.
The default folder is `~/.local/bin`. For a custom installation, replace the value
with the folder you passed as `SOTTO_INSTALL_DIR`, for example `"$HOME/Sotto CLI/bin"`.
Keep the double quotes so a path with spaces stays one argument:

```sh
dir="$HOME/.local/bin"
ls -l "$dir/sotto"
command -v sotto
```

If the listing shows the file you meant, remove just that file:

```sh
rm "$dir/sotto"
```

On Windows PowerShell the default folder is `%LOCALAPPDATA%\sotto\bin`. For a custom
installation, use your `SOTTO_INSTALL_DIR` folder instead:

```powershell
$dir = "$env:LOCALAPPDATA\sotto\bin"
Get-Item -LiteralPath "$dir\sotto.exe"
Get-Command sotto -ErrorAction SilentlyContinue
```

```powershell
Remove-Item -LiteralPath "$dir\sotto.exe"
```

Removing the executable does not touch your vault data, project files or Emergency
Kit. The local store lives in the operating system's application data folder, or in
`SOTTO_DATA_DIR` if you set it, and the commands above name one file only. Check that
`SOTTO_DATA_DIR` does not point at the install folder before you remove anything. Completion
scripts that either installer copied into your shell's completion folders, where those
folders existed, are separate files and are also left in place.

Then run the resolution check again:

```sh
command -v sotto
```

```powershell
Get-Command sotto -ErrorAction SilentlyContinue
```

If this prints a path, another installation (for example an older copy earlier on
`PATH`, or the Homebrew one) is now the one your shell finds. Run
`sotto --version` on it before relying on it. If nothing is printed, no `sotto` is
left on `PATH`.
