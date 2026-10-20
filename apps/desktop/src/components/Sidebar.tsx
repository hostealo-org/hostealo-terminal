// Sidebar laid out like the client panel v2: brand switcher on top, grouped navigation, and a
// settings block at the bottom with theme and language.
import { useState, type MouseEvent, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Activity, ArrowLeftRight, ChevronsUpDown, Cloud, ExternalLink, Fingerprint, Globe, KeyRound, Settings, SlidersHorizontal, SquareTerminal } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { cn } from "@hostealo/ui/cn";

import type { Host } from "../lib/ipc";
import { hasLocalShell, isIOS, isMac } from "../lib/format";
import { useAlerts } from "../store/alerts";
import { useHostealo } from "../store/hostealo";
import { useSettings, type LanguagePreference, type ThemePreference } from "../store/settings";
import { useTabs } from "../store/tabs";
import { useUi } from "../store/ui";
import { HostTree } from "./HostTree";
import { Logo } from "./Logo";
import { Menu, type MenuAnchor } from "./ui/Menu";
import packageJson from "../../package.json";

// From package.json, which each release bumps along with tauri.conf.json.
const APP_VERSION = packageJson.version;

interface SidebarProps {
  onNewLocal: () => void;
  onConnect: (host: Host, fresh?: boolean) => void;
  onOpenSftp: (host: Host) => void;
  onOpenPage: (page: "keys" | "knownHosts" | "status") => void;
  onNewHost: (folder: string | null) => void;
  onEditHost: (host: Host) => void;
  onDeleteHost: (host: Host) => void;
  onOpenSettings: () => void;
}

export function Sidebar(props: SidebarProps) {
  const { t } = useTranslation();
  const activeKind = useTabs((s) => s.tabs.find((tab) => tab.id === s.activeId)?.kind);
  const setSyncing = useUi((s) => s.setSyncOpen);
  const activeAlerts = useAlerts((s) => s.alerts.filter((a) => a.active).length);
  const connected = useTabs((s) => new Set(s.tabs.filter((tab) => tab.hostId && tab.status === "connected").map((tab) => tab.hostId)).size);

  return (
    <aside className="flex w-64 shrink-0 flex-col p-2 compact:h-full compact:w-full">
      {/* On macOS the traffic lights sit over this strip; it doubles as a window drag handle. */}
      {isMac && !isIOS && <div data-tauri-drag-region className="h-8 shrink-0" />}

      <BrandMenu />

      <nav className="mt-3 min-h-0 flex-1 space-y-5 overflow-x-hidden overflow-y-auto text-sm">
        <Group title={t("sidebar.sessions")}>
          {hasLocalShell && (
            <NavItem icon={<SquareTerminal />} onClick={props.onNewLocal}>
              {t("sidebar.localTerminal")}
            </NavItem>
          )}
          <NavItem
            icon={<Activity />}
            current={activeKind === "status"}
            onClick={() => props.onOpenPage("status")}
            badge={activeAlerts > 0 ? { text: String(activeAlerts), alert: true } : connected > 0 ? { text: String(connected) } : undefined}
          >
            {t("status.title")}
          </NavItem>
        </Group>

        <HostTree
          onConnect={props.onConnect}
          onOpenSftp={props.onOpenSftp}
          onNewHost={props.onNewHost}
          onEditHost={props.onEditHost}
          onDeleteHost={props.onDeleteHost}
        />

        <Group title={t("sidebar.tools")}>
          <NavItem icon={<KeyRound />} current={activeKind === "keys"} onClick={() => props.onOpenPage("keys")}>
            {t("keys.title")}
          </NavItem>
          <NavItem icon={<Fingerprint />} current={activeKind === "knownHosts"} onClick={() => props.onOpenPage("knownHosts")}>
            {t("knownHosts.title")}
          </NavItem>
          <NavItem icon={<ArrowLeftRight />} onClick={() => setSyncing(true)}>
            {t("sync.title")}
          </NavItem>
        </Group>
      </nav>

      <SettingsMenu onOpenSettings={props.onOpenSettings} />
    </aside>
  );
}

function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section>
      <p className="px-2 pb-1.5 text-xs font-medium text-fg-muted">{title}</p>
      <ul className="space-y-0.5">{children}</ul>
    </section>
  );
}

function NavItem({
  icon,
  current,
  onClick,
  badge,
  children,
}: {
  icon: ReactNode;
  current?: boolean;
  onClick: () => void;
  /** A count on the right; red for alerts. */
  badge?: { text: string; alert?: boolean };
  children: ReactNode;
}) {
  return (
    <li>
      <button
        type="button"
        onClick={onClick}
        aria-current={current ? "page" : undefined}
        className="v2-nav-link flex w-full cursor-pointer items-center gap-2.5 rounded-md px-2 py-1.5 text-left touch:py-2.5 [&>svg]:size-4 [&>svg]:shrink-0"
      >
        {icon}
        <span className="flex-1 truncate">{children}</span>
        {badge && (
          <span
            className={cn(
              "min-w-5 rounded-full px-1.5 text-center text-[11px] leading-5 font-medium tabular-nums",
              badge.alert ? "bg-red text-white" : "bg-muted text-fg-muted",
            )}
          >
            {badge.text}
          </span>
        )}
      </button>
    </li>
  );
}

const anchorOf = (e: MouseEvent, placement: "below" | "above"): MenuAnchor => ({
  rect: (e.currentTarget as HTMLElement).getBoundingClientRect(),
  placement,
});

/** Top of the sidebar: logo, name and a menu with Hostealo links (the panel's account switcher). */
function BrandMenu() {
  const { t } = useTranslation();
  const [anchor, setAnchor] = useState<MenuAnchor | null>(null);
  return (
    <>
      <button
        type="button"
        onClick={(e) => setAnchor(anchorOf(e, "below"))}
        aria-expanded={anchor !== null}
        className="v2-nav-link flex w-full cursor-pointer items-center gap-2.5 rounded-lg p-2 text-left"
      >
        <Logo />
        <span className="min-w-0 flex-1 leading-tight">
          <span className="block truncate text-sm font-semibold text-fg">{t("app.name")}</span>
          <span className="block truncate text-xs text-fg-muted">v{APP_VERSION} · Beta</span>
        </span>
        <ChevronsUpDown className="size-4 shrink-0 text-fg-muted" strokeWidth={1.8} />
      </button>
      {anchor && (
        <Menu
          anchor={anchor}
          onClose={() => setAnchor(null)}
          className="w-60"
          entries={[
            { heading: "Hostealo" },
            {
              label: useHostealo.getState().account ? t("hostealo.menuConnected") : t("hostealo.menuConnect"),
              icon: <Cloud />,
              onSelect: () => useHostealo.getState().setDialogOpen(true),
            },
            { label: t("brand.clientArea"), icon: <ExternalLink />, onSelect: () => void openUrl("https://my.hostealo.com") },
            { label: t("brand.website"), icon: <Globe />, onSelect: () => void openUrl("https://hostealo.es") },
            { separator: true },
            { label: t("brand.source"), icon: <ExternalLink />, onSelect: () => void openUrl("https://github.com/hostealo-org/hostealo-terminal") },
          ]}
        />
      )}
    </>
  );
}

/** Bottom of the sidebar: theme and language at hand, like the panel's user menu. */
function SettingsMenu({ onOpenSettings }: { onOpenSettings: () => void }) {
  const { t } = useTranslation();
  const { theme, language, update } = useSettings();
  const [anchor, setAnchor] = useState<MenuAnchor | null>(null);

  const themes: { value: ThemePreference; label: string }[] = [
    { value: "system", label: t("settings.auto") },
    { value: "light", label: t("settings.light") },
    { value: "dark", label: t("settings.dark") },
  ];
  const languages: { value: LanguagePreference; label: string }[] = [
    { value: "system", label: t("settings.auto") },
    { value: "es", label: "ES" },
    { value: "en", label: "EN" },
  ];

  return (
    <>
      <button
        type="button"
        onClick={(e) => setAnchor(anchorOf(e, "above"))}
        aria-expanded={anchor !== null}
        className="v2-nav-link mt-2 flex w-full cursor-pointer items-center gap-2.5 rounded-lg p-2 text-left"
      >
        <span className="grid size-8 shrink-0 place-items-center rounded-lg bg-accent-button text-white">
          <Settings className="size-4" />
        </span>
        <span className="min-w-0 flex-1 leading-tight">
          <span className="block truncate text-sm font-medium text-fg">{t("sidebar.settings")}</span>
          <span className="block truncate text-xs text-fg-muted">{t("settings.summary")}</span>
        </span>
        <ChevronsUpDown className="size-4 shrink-0 text-fg-muted" strokeWidth={1.8} />
      </button>
      {anchor && (
        <Menu
          anchor={anchor}
          onClose={() => setAnchor(null)}
          className="w-64"
          entries={[
            { content: <SegmentRow label={t("settings.theme")} value={theme} options={themes} onChange={(theme) => update({ theme })} /> },
            { content: <SegmentRow label={t("settings.language")} value={language} options={languages} onChange={(language) => update({ language })} /> },
            { separator: true },
            { label: t("settings.more"), icon: <SlidersHorizontal />, hint: isMac ? "⌘," : "Ctrl+Shift+,", onSelect: onOpenSettings },
          ]}
        />
      )}
    </>
  );
}

function SegmentRow<T extends string>({
  label,
  value,
  options,
  onChange,
}: {
  label: string;
  value: T;
  options: { value: T; label: string }[];
  onChange: (value: T) => void;
}) {
  return (
    <div className="flex items-center gap-2 px-2 py-1.5">
      <span className="flex-1 text-fg-secondary">{label}</span>
      <div className="v2-seg flex rounded-md p-0.5" role="group" aria-label={label}>
        {options.map((option) => (
          <button
            key={option.value}
            type="button"
            onClick={() => onChange(option.value)}
            aria-pressed={value === option.value}
            className={cn("cursor-pointer rounded px-1.5 py-0.5 text-xs", value === option.value && "is-on")}
          >
            {option.label}
          </button>
        ))}
      </div>
    </div>
  );
}
