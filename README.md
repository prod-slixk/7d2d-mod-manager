# 7D2D Mod Manager

A portable Windows mod manager for 7 Days to Die and its dedicated server.

## Download

Get `7D2D-Mod-Manager.exe` from the [latest release](../../releases/latest). There's nothing to install. Windows may show a SmartScreen warning because the exe isn't code-signed: choose **More info**, then **Run anyway**.

Settings are stored in `%LOCALAPPDATA%\7D2D Mod Manager`. To keep them next to the exe instead, create a folder named `data` beside it.

## Features

- **Install** zip, 7z or rar archives and mod folders by drag and drop, or from Nexus Mods' "Mod Manager Download" button. Every install is reviewed before anything changes.
- **Vanilla and modded play:** one switch turns every mod off for crossplay with console players (EAC on), and turns the same mods back on afterwards.
- **World tracking:** mark worlds as vanilla or modded. The manager warns before launching if the last world played doesn't match the current mods, backs up the other mode's worlds before each launch, and offers a restore when a world was played in the wrong mode.
- **Enable and disable** mods without deleting them. Disabled mods move to `Mods_Disabled`.
- **Conflict detection** across mods' XML patches, plus duplicate mods, broken XML and mods extracted one folder too deep.
- **Profiles and modpacks** that can be exported as `.7dmp` files and shared.
- **Launching** with Easy Anti-Cheat switched off automatically when a DLL mod needs it.
- **Backups** of mods and saves, and a game log viewer that highlights mod errors.

## Building

Requires Rust (MSVC toolchain), Node.js and the Visual Studio C++ build tools.

```bash
npm install
npx tauri build --no-bundle
```

The exe is written to `src-tauri/target/release/7D2D-Mod-Manager.exe`. Run the backend tests with `cargo test` from `src-tauri`.

Not affiliated with The Fun Pimps or Nexus Mods.
