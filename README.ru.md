# Round Studio Launcher

[English](./README.md) | [Русский](./README.ru.md)

Лаунчер Minecraft для Windows, сделанный на `Tauri + React + Rust`.

## Что Есть В Репозитории

- `src/App.tsx`: основной UI лаунчера и браузер контента
- `src/App.css`: цветовые переменные, layout, кнопки, адаптивность
- `src-tauri/src/lib.rs`: backend-команды для версий, установки, запуска, Modrinth и файлов
- `src-tauri/icons/`: иконки приложения для `exe` и установщиков
- `public/images/`: локальные изображения версий Minecraft внутри лаунчера
- `app-icon.png`: исходная иконка для генерации пакета Tauri-иконок

## Как Менять Дизайн

Если кто-то хочет быстро переделать внешний вид лаунчера:

1. Начать с `src/App.css`
2. Сначала поменять цвета в `:root`
3. Потом править layout в `.menu-card`, `.preview-frame`, `.menu-bottom`, `.mods-view`
4. Для вкладки контента менять блоки `.content-kind-*` и `.mods-*`
5. Для смены иконки заменить `app-icon.png` и выполнить:

```bash
npx tauri icon app-icon.png -o src-tauri/icons
```

## Что Нужно Для Сборки

- Node.js 20+
- Rust
- Tauri prerequisites для Windows
- Microsoft WebView2 Runtime

## Установка Зависимостей

```bash
npm install
```

## Запуск В Режиме Разработки

```bash
npm run tauri:dev
```

## Сборка

Только frontend:

```bash
npm run build
```

Полное desktop-приложение:

```bash
npm run tauri:build
```

## Основные Возможности

- официальный список версий Minecraft
- `vanilla`, `fabric`, `forge`
- offline-вход по нику
- 3D-превью скина
- установка и запуск по профилям
- браузер Modrinth для:
  - модов
  - ресурспаков
  - шейдерпаков
- популярные подборки контента и пагинация
- уведомления с прогрессом установки

## Примечания

- данные лаунчера хранятся в `%AppData%/RoundStudioLauncher`
- рантайм для шейдеров не ставится автоматически
- авторизация только offline
