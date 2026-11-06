# Cacaduu Upload

A small desktop app for a shop's computer (Windows `.exe`, macOS `.dmg`, each a few MB). It has one page,
**Export folder**, and runs in the background. Every 15 minutes it sends the newest stock file from the shop's export folder to
**cacaduu.com**.

It works like the website's **Môj obchod → Priečinok s exportom** page in Edge or Chrome, but it runs without a
browser window. It starts with the computer and stays in the tray (Windows) or the menu bar (macOS).

## What it does

1. The owner signs in with the shop's Cacaduu e-mail and password. Only a renewable login token is kept, in the
   system's secure storage (Windows DPAPI or the macOS Keychain). The password is never saved.
2. The owner chooses the shop (only if the account has more than one) and the export folder, e.g. `C:\Export`.
3. Every 15 minutes it finds the newest `XML`, `CSV`, `TXT` or `XLSX` file in that folder (not in subfolders) and
   sends it to Cacaduu, but only when the file is:
   - unchanged for at least a minute, so a half-written export is never sent,
   - newer than the last file Cacaduu applied,
   - not already sent while Cacaduu is still waiting for the columns to be approved.
4. Cacaduu's `stock-pull` function reads the file with the shop's approved columns, exactly as it does for the
   website's folder page. The first file has to have its columns approved once on cacaduu.com
   (**Môj obchod → Stĺpce súboru so zásobami**).

The folder is only ever read. Closing the window keeps the app running; **Quit** in the tray menu stops it.
**Check now** sends straight away.

If the shop's stock source on cacaduu.com is the POS agent, a connected inventory program or "Sklad v Cacaduu",
Cacaduu refuses files, and the app shows that message instead of sending anything.

## Getting the installers

GitHub Actions builds both on every push (see `.github/workflows/build.yml`). Open the repository's **Actions** tab,
then the latest **Build installers** run. Under **Artifacts** you will find:

- `Cacaduu-Upload-Windows` → `Cacaduu Upload_<version>_x64-setup.exe` (installs for the current user, no admin rights)
- `Cacaduu-Upload-macOS` → `Cacaduu Upload_<version>_universal.dmg` (Apple Silicon and Intel)

To publish them as a GitHub release, push a tag such as `v1.1.0`.

The installers are not signed with a paid certificate yet:

- **Windows:** SmartScreen may say "Windows protected your PC". Click **More info**, then **Run anyway**. The app
  uses Windows' own Edge WebView2 (already on Windows 10 and 11); on an old PC without it, the installer downloads it
  once.
- **macOS:** drag the app to Applications and open it. If macOS blocks it, go to **System Settings → Privacy &
  Security** and click **Open Anyway**.

## Development

Built with [Tauri 2](https://tauri.app): the page is plain HTML/CSS/JS, the background work is Rust, and the window
uses the system's own web view, which keeps the installers small.

```bash
npm install
npm test           # cargo test: the folder rules and the calls to Cacaduu (against a local stand-in)
npm start          # run the app
npm run build:win  # Windows .exe (on Windows)
npm run build:mac  # macOS universal .dmg (on a Mac)
```

- `src/page/` is the one page.
- `src-tauri/src/main.rs` holds the window, the tray, the login (the refresh token lives in Windows Credential
  Manager or the macOS Keychain) and the 15-minute loop.
- `src-tauri/src/sync.rs` holds the folder rules.
- `src-tauri/src/cloud.rs` holds the calls to Cacaduu.
- `src-tauri/strings.json` holds the texts (Slovak and English).

The app uses only Cacaduu's public settings: the Supabase project address and its publishable key. Everything is
checked on the server with the owner's own login (`my_shops`, `upload_check_in`, `stock-pull`). The API is described
in the [Cacaduu PPI](https://github.com/novidave/CACADOO-PPI) repository (`docs/SHOP_PC_SETUP.md`,
`supabase/functions/stock-pull`). This app does not change that repository.
