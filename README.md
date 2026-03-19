# Round Studio Launcher

[English](./README.md) | [Русский](./README.ru.md)

Minecraft launcher for Windows built with `Tauri + React + Rust`.

## What Is In The Repo

- `src/App.tsx`: main launcher UI and content browser
- `src/App.css`: design tokens, layout, buttons, responsive rules
- `src-tauri/src/lib.rs`: backend commands for versions, install, launch, Modrinth, files
- `src-tauri/icons/`: application icons used for the `exe` and installers
- `public/images/`: local version images shown in the launcher
- `app-icon.png`: source icon used to regenerate the Tauri icon pack

## Design Editing Guide

If someone wants to restyle the launcher:

1. Start in `src/App.css`
2. Change colors in the `:root` variables first
3. Tweak layout in `.menu-card`, `.preview-frame`, `.menu-bottom`, `.mods-view`
4. Adjust content browser appearance in `.content-kind-*`, `.mods-*`
5. Update icon by replacing `app-icon.png` and running:

```bash
npx tauri icon app-icon.png -o src-tauri/icons
```

## Requirements

- Node.js 20+
- Rust
- Tauri prerequisites for Windows
- Microsoft WebView2 Runtime

## Install Dependencies

```bash
npm install
```

## Run In Dev Mode

```bash
npm run tauri:dev
```

## Build

Frontend only:

```bash
npm run build
```

Desktop app:

```bash
npm run tauri:build
```

## Main Features

- official Minecraft version list
- `vanilla`, `fabric`, `forge`
- offline nickname-based login
- 3D skin preview
- install and launch per profile
- Modrinth browser for:
  - mods
  - resource packs
  - shader packs
- popular content lists and pagination
- install progress notifications

## Notes

- launcher data is stored in `%AppData%/RoundStudioLauncher`
- shader runtime is not auto-installed
- offline authorization only
