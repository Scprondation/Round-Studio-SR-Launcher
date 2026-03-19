import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { IdleAnimation, SkinViewer } from "skinview3d";
import "./App.css";

type LoaderType = "vanilla" | "fabric" | "forge";

type Profile = {
  id: string;
  name: string;
  minecraftVersion: string;
  loader: LoaderType;
  loaderVersion?: string | null;
  nickname: string;
  ramMb: number;
  javaPath?: string | null;
  jvmArgs: string[];
};

type ProfileSummary = {
  id: string;
  name: string;
  minecraftVersion: string;
  loader: LoaderType;
  loaderVersion?: string | null;
  nickname: string;
};

type MinecraftVersion = {
  id: string;
  versionType: string;
  releaseTime: string;
};

type LoaderOptions = {
  fabric: string[];
  forge: string[];
};

type ContentKind = "mod" | "resourcepack" | "shaderpack";

type ContentItem = {
  id: string;
  fileName: string;
  kind: ContentKind;
  source: string;
  addedAt: string;
};

type ModrinthSearchHit = {
  projectId: string;
  slug: string;
  title: string;
  description: string;
};

type ModrinthSearchPage = {
  hits: ModrinthSearchHit[];
  totalHits: number;
  page: number;
  pageSize: number;
};

type ValidationIssue = {
  severity: "warning" | "critical";
  code: string;
  message: string;
};

type ValidationReport = {
  canLaunch: boolean;
  critical: ValidationIssue[];
  warnings: ValidationIssue[];
};

type InstallProgressPayload = {
  step: number;
  total: number;
  percent: number;
  message: string;
};

type NoticeType = "yellow" | "green" | "red";

type Notice = {
  id: string;
  type: NoticeType;
  message: string;
  mode: "timed" | "download";
  remainingMs: number;
  durationMs: number;
  remainingPercent: number;
};

type ModResultsMode = "popular" | "search";

const TICK_MS = 100;
const NICK_RE = /^[A-Za-z0-9_]{3,16}$/;

// UI copy helpers for the content browser.
// If you rename tabs or add a new content type, start here.
function contentKindLabel(kind: ContentKind): string {
  switch (kind) {
    case "mod":
      return "мод";
    case "resourcepack":
      return "ресурспак";
    case "shaderpack":
      return "шейдер";
  }
}

function contentKindPlural(kind: ContentKind): string {
  switch (kind) {
    case "mod":
      return "моды";
    case "resourcepack":
      return "ресурспаки";
    case "shaderpack":
      return "шейдеры";
  }
}

function contentPlaceholder(kind: ContentKind): string {
  switch (kind) {
    case "mod":
      return "sodium, iris, journeymap...";
    case "resourcepack":
      return "faithful, better leaves, fresh animations...";
    case "shaderpack":
      return "complementary, bsl, iterationt...";
  }
}

function contentNeedsLoader(kind: ContentKind): boolean {
  return kind === "mod";
}

function parseMc(version: string): { major: number; minor: number } | null {
  const parts = version.split(".");
  if (parts.length < 2) return null;
  const major = Number(parts[0]);
  const minor = Number(parts[1]);
  if (Number.isNaN(major) || Number.isNaN(minor)) return null;
  return { major, minor };
}

function isLegacyVersion(version: string): boolean {
  const parsed = parseMc(version);
  if (!parsed) return false;
  return parsed.major === 1 && parsed.minor <= 12;
}

function versionImage(version: string): string {
  if (isLegacyVersion(version)) return "/images/images.jpg";
  return `/images/${version}.jpg`;
}

function skinUrlByNickname(nickname: string): string {
  const safe = nickname.trim();
  const base = safe
    ? `https://minotar.net/skin/${encodeURIComponent(safe)}`
    : "https://minotar.net/skin/Steve";
  return `${base}?v=${encodeURIComponent(safe.toLowerCase())}`;
}

function formatLaunchError(error: unknown): string {
  const text = String(error);
  if (text.toLowerCase().includes("java")) {
    return "ошибка java. проверь путь к java и jvm args";
  }
  return text;
}

function extractLaunchFailureMessage(lines: string[]): string {
  const cleaned = lines.map((x) => x.trim()).filter(Boolean);
  if (!cleaned.length) {
    return "игра завершилась с ошибкой";
  }

  for (let i = cleaned.length - 1; i >= 0; i -= 1) {
    const line = cleaned[i];
    if (/^Caused by:/i.test(line)) {
      return line;
    }
  }
  for (let i = 0; i < cleaned.length; i += 1) {
    const line = cleaned[i];
    if (/Exception|Error|Could not|NoClassDefFoundError|ClassNotFoundException/i.test(line)) {
      return line;
    }
  }
  return cleaned[cleaned.length - 1];
}

function App() {
  // Main UI state. These values control what the player sees in the launcher.
  const [activeTab, setActiveTab] = useState<"launcher" | "mods">("launcher");
  const [status, setStatus] = useState("готово");
  const [versions, setVersions] = useState<MinecraftVersion[]>([]);
  const [selectedVersion, setSelectedVersion] = useState("1.21.4");
  const [selectedLoader, setSelectedLoader] = useState<LoaderType>("vanilla");
  const [selectedLoaderVersion, setSelectedLoaderVersion] = useState("");
  const [loaderOptions, setLoaderOptions] = useState<LoaderOptions>({ fabric: [], forge: [] });
  const [nickname, setNickname] = useState("Player123");
  const [profiles, setProfiles] = useState<ProfileSummary[]>([]);
  const [profile, setProfile] = useState<Profile | null>(null);
  const [runningHandle, setRunningHandle] = useState<number | null>(null);
  const [notices, setNotices] = useState<Notice[]>([]);
  const [currentImageSrc, setCurrentImageSrc] = useState(versionImage("1.21.4"));
  const [contentKind, setContentKind] = useState<ContentKind>("mod");
  const [contentQuery, setContentQuery] = useState("");
  const [contentResults, setContentResults] = useState<ModrinthSearchHit[]>([]);
  const [installedContent, setInstalledContent] = useState<ContentItem[]>([]);
  const [modResultsMode, setModResultsMode] = useState<ModResultsMode>("popular");
  const [contentPage, setContentPage] = useState(1);
  const [contentPageSize, setContentPageSize] = useState(20);
  const [contentTotalHits, setContentTotalHits] = useState(0);
  const [contentLoading, setContentLoading] = useState(false);
  const [contentInstallingProjectId, setContentInstallingProjectId] = useState<string | null>(null);

  const downloadNoticeIdRef = useRef<string | null>(null);
  const launchStderrLinesRef = useRef<string[]>([]);
  const preferredLoaderVersionRef = useRef("");
  const skinCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const skinViewerRef = useRef<SkinViewer | null>(null);

  const releaseVersions = useMemo(
    () => versions.filter((item) => item.versionType === "release"),
    [versions],
  );
  const activeLoaderVersions = useMemo(() => {
    if (selectedLoader === "fabric") return loaderOptions.fabric;
    if (selectedLoader === "forge") return loaderOptions.forge;
    return [];
  }, [loaderOptions, selectedLoader]);
  const contentTotalPages = useMemo(
    () => Math.max(1, Math.ceil(contentTotalHits / Math.max(1, contentPageSize))),
    [contentPageSize, contentTotalHits],
  );

  const pushTimedNotice = useCallback((type: NoticeType, message: string, durationMs = 3200) => {
    const id = crypto.randomUUID();
    setNotices((prev) => [
      {
        id,
        type,
        message,
        mode: "timed",
        remainingMs: durationMs,
        durationMs,
        remainingPercent: 100,
      },
      ...prev,
    ]);
  }, []);

  const showDownloadNotice = useCallback((message: string) => {
    const existingId = downloadNoticeIdRef.current;
    if (existingId) {
      setNotices((prev) =>
        prev.map((n) =>
          n.id === existingId
            ? {
                ...n,
                type: "yellow",
                message,
                mode: "download",
                remainingPercent: 100,
              }
            : n,
        ),
      );
      return existingId;
    }

    const id = crypto.randomUUID();
    downloadNoticeIdRef.current = id;
    setNotices((prev) => [
      {
        id,
        type: "yellow",
        message,
        mode: "download",
        remainingMs: 0,
        durationMs: 0,
        remainingPercent: 100,
      },
      ...prev,
    ]);
    return id;
  }, []);

  const updateDownloadNotice = useCallback((percentDownloaded: number, message: string) => {
    const id = downloadNoticeIdRef.current;
    if (!id) return;
    const remainingPercent = Math.max(0, Math.min(100, 100 - percentDownloaded));
    setNotices((prev) =>
      prev.map((n) =>
        n.id === id
          ? {
              ...n,
              type: "yellow",
              message,
              mode: "download",
              remainingPercent,
            }
          : n,
      ),
    );
  }, []);

  const clearDownloadNotice = useCallback(() => {
    const id = downloadNoticeIdRef.current;
    if (!id) return;
    setNotices((prev) => prev.filter((n) => n.id !== id));
    downloadNoticeIdRef.current = null;
  }, []);

  useEffect(() => {
    const timer = window.setInterval(() => {
      setNotices((prev) =>
        prev
          .map((n) => {
            if (n.mode !== "timed") return n;
            const nextRemaining = n.remainingMs - TICK_MS;
            if (nextRemaining <= 0) {
              return null;
            }
            return {
              ...n,
              remainingMs: nextRemaining,
              remainingPercent: (nextRemaining / n.durationMs) * 100,
            };
          })
          .filter((n): n is Notice => n !== null),
      );
    }, TICK_MS);

    return () => window.clearInterval(timer);
  }, []);

  const refreshVersions = useCallback(async () => {
    const list = await invoke<MinecraftVersion[]>("versions_list_minecraft");
    setVersions(list);
    const releases = list.filter((item) => item.versionType === "release");
    if (releases.length > 0) {
      setSelectedVersion((prev) => (releases.some((r) => r.id === prev) ? prev : releases[0].id));
    }
  }, []);

  const refreshProfiles = useCallback(async () => {
    const list = await invoke<ProfileSummary[]>("profiles_list");
    setProfiles(list);
    if (!list.length) {
      setProfile(null);
      setNickname("Player123");
      return;
    }
    const main = list.find((item) => item.name.toLowerCase() === "main") ?? list[0];
    const full = await invoke<Profile>("profiles_get", { profileId: main.id });
    setProfile(full);
    setNickname(full.nickname);
    setSelectedVersion(full.minecraftVersion);
    setSelectedLoader(full.loader);
    setSelectedLoaderVersion(full.loaderVersion ?? "");
  }, []);

  const refreshInstalledContent = useCallback(async (profileId?: string) => {
    const resolvedProfileId = profileId ?? profile?.id;
    if (!resolvedProfileId) {
      setInstalledContent([]);
      return;
    }
    const items = await invoke<ContentItem[]>("content_list", { profileId: resolvedProfileId });
    setInstalledContent(items.filter((item) => item.kind === contentKind));
  }, [contentKind, profile?.id]);

  const refreshLoaderOptions = useCallback(
    async (minecraftVersion: string, loader: LoaderType, preferredVersion?: string | null) => {
      if (loader === "vanilla") {
        setLoaderOptions({ fabric: [], forge: [] });
        setSelectedLoaderVersion("");
        return;
      }
      const options = await invoke<LoaderOptions>("loaders_for_version", { minecraftVersion });
      setLoaderOptions(options);
      const versionsForLoader = loader === "fabric" ? options.fabric : options.forge;
      const preferred = (preferredVersion ?? "").trim();
      const next = preferred && versionsForLoader.includes(preferred) ? preferred : versionsForLoader[0] ?? "";
      setSelectedLoaderVersion(next);
    },
    [],
  );

  useEffect(() => {
    void refreshVersions().catch((e) => setStatus(String(e)));
    void refreshProfiles().catch((e) => setStatus(String(e)));

    const unlistenPromise = Promise.all([
      listen<InstallProgressPayload | string>("install/progress", (event) => {
        if (typeof event.payload === "string") {
          setStatus(event.payload);
          showDownloadNotice(event.payload.toLowerCase());
          return;
        }
        const percent = Math.max(0, Math.min(100, Math.round(event.payload.percent)));
        setStatus(event.payload.message);
        updateDownloadNotice(percent, event.payload.message.toLowerCase());
      }),
      listen<string>("download/error", (event) => {
        setStatus(event.payload);
        clearDownloadNotice();
        pushTimedNotice("red", event.payload.toLowerCase());
      }),
      listen<string>("launch/log", (event) => {
        const line = event.payload.trim();
        if (line.startsWith("[stderr]")) {
          const text = line.replace("[stderr]", "").trim();
          if (text) {
            launchStderrLinesRef.current = [...launchStderrLinesRef.current.slice(-24), text];
          }
        }
      }),
      listen<string>("launch/state", (event) => {
        const msg = event.payload;
        if (msg.startsWith("started:")) {
          setRunningHandle(Number(msg.split(":")[1]));
          launchStderrLinesRef.current = [];
          setStatus("игра запущена");
          pushTimedNotice("green", "игра запущена");
        }
        if (msg.startsWith("stopped:")) {
          setRunningHandle(null);
          setStatus("игра остановлена");
          pushTimedNotice("yellow", "игра остановлена");
        }
        if (msg.startsWith("exited:")) {
          const [, idRaw, codeRaw] = msg.split(":");
          const id = Number(idRaw);
          const code = Number(codeRaw);
          if (!Number.isNaN(id)) {
            setRunningHandle((current) => (current === id ? null : current));
          }
          if (code === 0) {
            launchStderrLinesRef.current = [];
            setStatus("игра закрыта");
            pushTimedNotice("yellow", "игра закрыта");
          } else {
            const reason = launchStderrLinesRef.current.length
              ? extractLaunchFailureMessage(launchStderrLinesRef.current)
              : `игра завершилась с ошибкой (${code})`;
            setStatus(reason);
            pushTimedNotice("red", reason);
          }
        }
      }),
    ]);

    return () => {
      void unlistenPromise.then((list) => list.forEach((unlisten) => unlisten()));
    };
  }, [clearDownloadNotice, pushTimedNotice, refreshProfiles, refreshVersions, showDownloadNotice, updateDownloadNotice]);

  useEffect(() => {
    void refreshInstalledContent().catch((error) => setStatus(String(error)));
  }, [refreshInstalledContent]);

  useEffect(() => {
    setCurrentImageSrc(versionImage(selectedVersion));
  }, [selectedVersion]);

  useEffect(() => {
    preferredLoaderVersionRef.current = selectedLoaderVersion;
  }, [selectedLoaderVersion]);

  useEffect(() => {
    void refreshLoaderOptions(selectedVersion, selectedLoader, preferredLoaderVersionRef.current).catch((error) => {
      setStatus(String(error));
      setLoaderOptions({ fabric: [], forge: [] });
      if (selectedLoader !== "vanilla") {
        setSelectedLoaderVersion("");
      }
    });
  }, [refreshLoaderOptions, selectedLoader, selectedVersion]);

  useEffect(() => {
    const canvas = skinCanvasRef.current;
    if (!canvas) return;

    const viewer = new SkinViewer({
      canvas,
      width: 280,
      height: 320,
      skin: skinUrlByNickname("Player123"),
    });
    viewer.animation = new IdleAnimation();
    viewer.autoRotate = true;
    viewer.autoRotateSpeed = 0.8;
    viewer.zoom = 0.8;
    skinViewerRef.current = viewer;

    return () => {
      viewer.dispose();
      if (skinViewerRef.current === viewer) {
        skinViewerRef.current = null;
      }
    };
  }, []);

  useEffect(() => {
    const viewer = skinViewerRef.current;
    if (!viewer) return;
    void viewer.loadSkin(skinUrlByNickname(nickname));
  }, [nickname]);

  async function ensureMainProfile(currentNickname: string): Promise<Profile> {
    if (profile) {
      return profile;
    }

    const mainSummary = profiles.find((item) => item.name.toLowerCase() === "main") ?? profiles[0];
    if (mainSummary) {
      const full = await invoke<Profile>("profiles_get", { profileId: mainSummary.id });
      setProfile(full);
      setNickname(full.nickname);
      return full;
    }

    const created = await invoke<Profile>("profiles_create", {
      input: {
        name: "Main",
        minecraftVersion: selectedVersion,
        loader: selectedLoader,
        loaderVersion: selectedLoader === "vanilla" ? null : selectedLoaderVersion || null,
        nickname: currentNickname,
        ramMb: 4096,
      },
    });
    setProfile(created);
    setNickname(created.nickname);
    await refreshProfiles();
    await refreshInstalledContent(created.id);
    return created;
  }

  async function syncProfile(base: Profile, currentNickname: string): Promise<Profile> {
    if (
      base.minecraftVersion === selectedVersion &&
      base.loader === selectedLoader &&
      (base.loaderVersion ?? "") === (selectedLoader === "vanilla" ? "" : selectedLoaderVersion) &&
      base.nickname === currentNickname
    ) {
      return base;
    }
    await invoke<Profile>("profiles_update", {
      profileId: base.id,
      patch: {
        minecraftVersion: selectedVersion,
        loader: selectedLoader,
        loaderVersion: selectedLoader === "vanilla" ? "" : selectedLoaderVersion,
        nickname: currentNickname,
      },
    });
    const next = await invoke<Profile>("profiles_get", { profileId: base.id });
    setProfile(next);
    setNickname(next.nickname);
    await refreshInstalledContent(next.id);
    return next;
  }

  // Shared loader for Modrinth lists.
  // The same block powers mods, resource packs and shaders, so design changes only need one place.
  const loadContentCatalog = useCallback(
    async (query: string, mode: ModResultsMode, page: number) => {
      if (contentNeedsLoader(contentKind) && selectedLoader === "vanilla") {
        setContentResults([]);
        setContentTotalHits(0);
        setContentPage(page);
        setModResultsMode("popular");
        return;
      }

      try {
        setContentLoading(true);
        setStatus(
          mode === "popular"
            ? `загрузка подборки: ${contentKindPlural(contentKind)}...`
            : `поиск: ${contentKindPlural(contentKind)}...`,
        );
        const result = await invoke<ModrinthSearchPage>("content_search_modrinth", {
          kind: contentKind,
          query,
          version: selectedVersion,
          loader: selectedLoader,
          sortIndex: mode === "popular" ? "downloads" : "relevance",
          page,
        });
        setContentResults(result.hits);
        setContentTotalHits(result.totalHits);
        setContentPage(result.page);
        setContentPageSize(result.pageSize);
        setModResultsMode(mode);
        setStatus(
          mode === "popular"
            ? `популярных элементов: ${result.hits.length}`
            : `найдено элементов: ${result.totalHits || result.hits.length}`,
        );
      } catch (error) {
        const message = String(error).toLowerCase();
        pushTimedNotice("red", message);
        setStatus(message);
      } finally {
        setContentLoading(false);
      }
    },
    [contentKind, pushTimedNotice, selectedLoader, selectedVersion],
  );

  const loadPopularContent = useCallback(
    async (page = 1) => {
      await loadContentCatalog("", "popular", page);
    },
    [loadContentCatalog],
  );

  useEffect(() => {
    if (activeTab !== "mods") return;
    if (contentNeedsLoader(contentKind) && selectedLoader === "vanilla") {
      setContentResults([]);
      setContentTotalHits(0);
      setContentPage(1);
      setModResultsMode("popular");
      return;
    }
    void loadPopularContent(1);
  }, [activeTab, contentKind, loadPopularContent, selectedLoader, selectedVersion]);

  async function searchContent(page = 1) {
    try {
      if (!contentQuery.trim()) {
        await loadPopularContent(page);
        return;
      }
      if (contentNeedsLoader(contentKind) && selectedLoader === "vanilla") {
        const message = "для модов выбери fabric или forge";
        pushTimedNotice("yellow", message);
        setStatus(message);
        return;
      }
      await loadContentCatalog(contentQuery.trim(), "search", page);
    } catch (error) {
      const message = String(error).toLowerCase();
      pushTimedNotice("red", message);
      setStatus(message);
    }
  }

  async function installContent(projectId: string) {
    try {
      if (contentNeedsLoader(contentKind) && selectedLoader === "vanilla") {
        const message = "моды требуют fabric или forge";
        pushTimedNotice("yellow", message);
        setStatus(message);
        return;
      }
      const currentNickname = nickname.trim();
      const ensured = await ensureMainProfile(currentNickname || "Player123");
      const active = await syncProfile(ensured, currentNickname || ensured.nickname);
      setContentInstallingProjectId(projectId);
      setStatus(`установка: ${contentKindLabel(contentKind)}...`);
      showDownloadNotice(`скачивание: ${contentKindLabel(contentKind)}`);
      await invoke<ContentItem>("content_install_modrinth", {
        profileId: active.id,
        projectId,
        versionId: "",
        kind: contentKind,
      });
      clearDownloadNotice();
      await refreshInstalledContent(active.id);
      pushTimedNotice("green", `${contentKindLabel(contentKind)} установлен`);
      setStatus(`${contentKindLabel(contentKind)} установлен`);
    } catch (error) {
      clearDownloadNotice();
      const message = String(error).toLowerCase();
      pushTimedNotice("red", message);
      setStatus(message);
    } finally {
      setContentInstallingProjectId(null);
    }
  }

  async function play() {
    try {
      const currentNickname = nickname.trim();
      if (!NICK_RE.test(currentNickname)) {
        const message = "ник: 3-16 символов, только латиница/цифры/_";
        pushTimedNotice("red", message);
        setStatus(message);
        return;
      }

      setStatus("подготовка...");
      const ensured = await ensureMainProfile(currentNickname);
      const active = await syncProfile(ensured, currentNickname);

      const report = await invoke<ValidationReport>("profile_validate", {
        profileId: active.id,
      });

      const loaderProblem = report.critical.find((i) => i.code === "MISSING_LOADER_VERSION");
      if (loaderProblem) {
        pushTimedNotice("red", "выбери версию загрузчика");
        setStatus(loaderProblem.message);
        return;
      }

      const javaProblem = report.critical.find((i) => i.code === "JAVA_NOT_FOUND");
      if (javaProblem) {
        pushTimedNotice("red", "java не найдена. укажи java в настройках профиля");
        setStatus(javaProblem.message);
        return;
      }

      const needInstall = report.critical.some(
        (i) =>
          i.code === "MISSING_CLIENT_JAR" ||
          i.code === "MISSING_LIBRARIES" ||
          i.code === "MISSING_NATIVES" ||
          i.code === "MISSING_ASSET_INDEX" ||
          i.code === "MISSING_ASSETS" ||
          i.code === "MISSING_LOADER_VERSION",
      );
      if (needInstall) {
        showDownloadNotice("загрузка версии");
        await invoke("profile_install", { profileId: active.id });
        clearDownloadNotice();
        pushTimedNotice("green", "версия скачана");
      }

      const finalReport = needInstall
        ? await invoke<ValidationReport>("profile_validate", { profileId: active.id })
        : report;

      if (!finalReport.canLaunch) {
        const firstCritical = finalReport.critical[0];
        const message = firstCritical?.message ?? "не удалось пройти валидацию профиля";
        pushTimedNotice("red", message.toLowerCase());
        setStatus(message);
        return;
      }

      const handle = await invoke<{ id: number }>("profile_launch", {
        profileId: active.id,
      });
      launchStderrLinesRef.current = [];
      setRunningHandle(handle.id);
      setStatus("запуск клиента...");
    } catch (error) {
      clearDownloadNotice();
      const message = formatLaunchError(error).toLowerCase();
      pushTimedNotice("red", message);
      setStatus(message);
    }
  }

  return (
    <div className="launcher-root">
      {/* Top-level shell. Most visual redesigns start in App.css under .launcher-root and .menu-card. */}
      <main className="menu-wrap">
        <section className="menu-card">
          <header className="menu-top">
            <h1>{activeTab === "launcher" ? `версия: ${selectedVersion}` : "контент modrinth"}</h1>
            <div className="menu-tabs">
              <button
                type="button"
                className={activeTab === "launcher" ? "menu-tab is-active" : "menu-tab"}
                onClick={() => setActiveTab("launcher")}
              >
                игра
              </button>
              <button
                type="button"
                className={activeTab === "mods" ? "menu-tab is-active" : "menu-tab"}
                onClick={() => setActiveTab("mods")}
              >
                моды
              </button>
            </div>
          </header>

          {activeTab === "launcher" ? (
            <>
              {/* Launcher home screen: version art, 3D skin preview and play controls. */}
              <div className="preview-frame">
                <div className="preview-image">
                  <img
                    src={currentImageSrc}
                    alt={`minecraft ${selectedVersion}`}
                    onError={() => setCurrentImageSrc("/images/images.jpg")}
                  />
                </div>
                <div className="skin-preview">
                  <canvas ref={skinCanvasRef} width={280} height={320} />
                  <span>3d скин</span>
                </div>
              </div>

              <footer className="menu-bottom">
                <div className="menu-controls">
                  <label>
                    <span>ник</span>
                    <input
                      type="text"
                      value={nickname}
                      maxLength={16}
                      disabled={runningHandle !== null}
                      onChange={(event) => setNickname(event.target.value)}
                      placeholder="Player123"
                    />
                  </label>
                  <label>
                    <span>выбор версии</span>
                    <select
                      value={selectedVersion}
                      disabled={runningHandle !== null}
                      onChange={(event) => setSelectedVersion(event.target.value)}
                    >
                      {releaseVersions.map((item) => (
                        <option key={item.id} value={item.id}>
                          {item.id}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label>
                    <span>загрузчик</span>
                    <select
                      value={selectedLoader}
                      disabled={runningHandle !== null}
                      onChange={(event) => setSelectedLoader(event.target.value as LoaderType)}
                    >
                      <option value="vanilla">vanilla</option>
                      <option value="fabric">fabric</option>
                      <option value="forge">forge</option>
                    </select>
                  </label>
                  {selectedLoader !== "vanilla" ? (
                    <label>
                      <span>версия загрузчика</span>
                      <select
                        value={selectedLoaderVersion}
                        disabled={runningHandle !== null || activeLoaderVersions.length === 0}
                        onChange={(event) => setSelectedLoaderVersion(event.target.value)}
                      >
                        {activeLoaderVersions.length === 0 ? (
                          <option value="">нет совместимых версий</option>
                        ) : (
                          activeLoaderVersions.map((item) => (
                            <option key={item} value={item}>
                              {item}
                            </option>
                          ))
                        )}
                      </select>
                    </label>
                  ) : null}
                </div>

                <button
                  type="button"
                  className="play-button"
                  onClick={() => void play()}
                  disabled={runningHandle !== null}
                >
                  {runningHandle ? "запущено" : "играть"}
                </button>
              </footer>
            </>
          ) : (
            <section className="mods-view">
              {/* Shared content browser for Modrinth categories. */}
              <div className="content-kind-tabs" role="tablist" aria-label="категория контента">
                {(["mod", "resourcepack", "shaderpack"] as const).map((kind) => (
                  <button
                    key={kind}
                    type="button"
                    className={contentKind === kind ? "content-kind-tab is-active" : "content-kind-tab"}
                    onClick={() => {
                      setContentKind(kind);
                      setContentQuery("");
                      setContentPage(1);
                    }}
                  >
                    {contentKindPlural(kind)}
                  </button>
                ))}
              </div>

              <div className="mods-toolbar">
                <label className="mods-search">
                  <span>{`поиск: ${contentKindLabel(contentKind)}`}</span>
                  <input
                    type="text"
                    value={contentQuery}
                    placeholder={contentPlaceholder(contentKind)}
                    onChange={(event) => setContentQuery(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") {
                        event.preventDefault();
                        void searchContent(1);
                      }
                    }}
                  />
                </label>
                <label>
                  <span>версия minecraft</span>
                  <select
                    value={selectedVersion}
                    onChange={(event) => setSelectedVersion(event.target.value)}
                  >
                    {releaseVersions.map((item) => (
                      <option key={item.id} value={item.id}>
                        {item.id}
                      </option>
                    ))}
                  </select>
                </label>
                <label>
                  <span>загрузчик</span>
                  <select
                    value={selectedLoader}
                    onChange={(event) => setSelectedLoader(event.target.value as LoaderType)}
                  >
                    <option value="vanilla">vanilla</option>
                    <option value="fabric">fabric</option>
                    <option value="forge">forge</option>
                  </select>
                </label>
                <button
                  type="button"
                  className="mods-search-button"
                  onClick={() => void searchContent(1)}
                  disabled={contentLoading}
                >
                  {contentLoading ? "поиск..." : "найти"}
                </button>
              </div>

              {contentNeedsLoader(contentKind) && selectedLoader === "vanilla" ? (
                <div className="mods-warning">
                  для модов нужен `fabric` или `forge`. сейчас выбран vanilla, поэтому установка модов
                  отключена.
                </div>
              ) : null}

              <div className="mods-grid">
                <section className="mods-panel">
                  <div className="mods-panel-head">
                    <h2>
                      {modResultsMode === "popular"
                        ? `популярные ${contentKindPlural(contentKind)}`
                        : `результаты: ${contentKindPlural(contentKind)}`}
                    </h2>
                    <span>{contentResults.length}</span>
                  </div>
                  <p className="mods-panel-copy">
                    {modResultsMode === "popular"
                      ? `подборка по скачиваниям для ${selectedVersion}${contentNeedsLoader(contentKind) ? ` / ${selectedLoader}` : ""}`
                      : `запрос: ${contentQuery.trim() || "без текста"}`}
                  </p>
                  <div className="mods-list">
                    {contentResults.length === 0 ? (
                      <div className="mods-empty">
                        {modResultsMode === "popular"
                          ? `modrinth не вернул популярные ${contentKindPlural(contentKind)} для этой версии.`
                          : "ничего не найдено. попробуй другое название или очисти поиск, чтобы увидеть популярную подборку."}
                      </div>
                    ) : (
                      contentResults.map((item) => (
                        <article key={item.projectId} className="mod-card">
                          <div className="mod-copy">
                            <strong>{item.title}</strong>
                            <span>{item.slug}</span>
                            <p>{item.description || "описание не указано"}</p>
                          </div>
                          <button
                            type="button"
                            className="mod-install-button"
                            disabled={
                              contentInstallingProjectId === item.projectId ||
                              (contentNeedsLoader(contentKind) && selectedLoader === "vanilla")
                            }
                            onClick={() => void installContent(item.projectId)}
                          >
                            {contentInstallingProjectId === item.projectId ? "ставим..." : "установить"}
                          </button>
                        </article>
                      ))
                    )}
                  </div>
                  <div className="mods-pagination">
                    <button
                      type="button"
                      className="mods-page-button"
                      disabled={contentLoading || contentPage <= 1}
                      onClick={() =>
                        void (modResultsMode === "popular"
                          ? loadPopularContent(contentPage - 1)
                          : searchContent(contentPage - 1))
                      }
                    >
                      назад
                    </button>
                    <span>{`страница ${contentPage} / ${contentTotalPages}`}</span>
                    <button
                      type="button"
                      className="mods-page-button"
                      disabled={contentLoading || contentPage >= contentTotalPages}
                      onClick={() =>
                        void (modResultsMode === "popular"
                          ? loadPopularContent(contentPage + 1)
                          : searchContent(contentPage + 1))
                      }
                    >
                      вперёд
                    </button>
                  </div>
                </section>

                <section className="mods-panel">
                  <div className="mods-panel-head">
                    <h2>установлено</h2>
                    <span>{installedContent.length}</span>
                  </div>
                  <div className="mods-list">
                    {installedContent.length === 0 ? (
                      <div className="mods-empty">{`в этом профиле пока нет: ${contentKindPlural(contentKind)}.`}</div>
                    ) : (
                      installedContent.map((item) => (
                        <article key={item.id} className="installed-mod-card">
                          <strong>{item.fileName}</strong>
                          <span>{item.source}</span>
                        </article>
                      ))
                    )}
                  </div>
                </section>
              </div>
            </section>
          )}
        </section>
      </main>

      <aside className="notice-stack">
        {notices.map((notice) => (
          <article key={notice.id} className={`notice notice-${notice.type}`}>
            <div className="notice-text">{notice.message}</div>
            <div className="notice-bar">
              <div style={{ width: `${Math.max(0, Math.min(100, notice.remainingPercent))}%` }} />
            </div>
          </article>
        ))}
      </aside>

      <div className="status-line">{status}</div>
    </div>
  );
}

export default App;
