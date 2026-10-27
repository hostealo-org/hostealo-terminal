import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { LoaderCircle, Minus, Plus, Settings } from "lucide-react";

import { isIOS, isMac } from "../lib/format";
import { deviceUnlock } from "../lib/ipc";
import { canUpdate, useUpdates } from "../lib/updates";
import { APP_VERSION } from "../lib/version";
import { useSettings, type LanguagePreference, type ThemePreference } from "../store/settings";
import { Button } from "./ui/Button";
import { Checkbox, Segmented } from "./ui/Form";
import { Modal } from "./ui/Modal";

const MIN_FONT = 10;
const MAX_FONT = 24;

export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const { t } = useTranslation();
  const { theme, language, fontSize, highlightOutput, copyOnSelect, screenReader, appLock, alertsEnabled, alertsNotify, alertDisk, alertMemory, alertCpu, update } = useSettings();
  const [lockError, setLockError] = useState<string | null>(null);
  // Turning the lock on asks once first, so it is never set where it cannot be opened.
  const setAppLock = async (on: boolean) => {
    setLockError(null);
    if (!on) return update({ appLock: false });
    try {
      await deviceUnlock(t("lock.reason"));
      update({ appLock: true });
    } catch (e) {
      if (String(e) !== "cancelled") setLockError(String(e));
    }
  };

  return (
    <Modal title={t("settings.title")} icon={<Settings className="size-4" />} onClose={onClose}>
      <div className="flex flex-col gap-5">
        <Section label={t("settings.language")}>
          <Segmented<LanguagePreference>
            value={language}
            onChange={(language) => update({ language })}
            options={[
              { value: "system", label: t("settings.system") },
              { value: "en", label: "English" },
              { value: "es", label: "Español" },
            ]}
          />
        </Section>

        <Section label={t("settings.theme")}>
          <Segmented<ThemePreference>
            value={theme}
            onChange={(theme) => update({ theme })}
            options={[
              { value: "system", label: t("settings.system") },
              { value: "dark", label: t("settings.dark") },
              { value: "light", label: t("settings.light") },
            ]}
          />
        </Section>

        <Section label={t("settings.fontSize")}>
          <div className="flex items-center gap-2">
            <Button size="icon" onClick={() => update({ fontSize: Math.max(MIN_FONT, fontSize - 1) })} aria-label="-">
              <Minus className="size-3.5" />
            </Button>
            <span className="w-10 text-center text-sm tabular-nums text-fg">{fontSize}px</span>
            <Button size="icon" onClick={() => update({ fontSize: Math.min(MAX_FONT, fontSize + 1) })} aria-label="+">
              <Plus className="size-3.5" />
            </Button>
          </div>
        </Section>

        <div className="flex flex-col gap-1.5">
          <Checkbox checked={highlightOutput} onChange={(highlightOutput) => update({ highlightOutput })}>
            <span className="font-medium text-fg">{t("settings.highlight")}</span>
          </Checkbox>
          <p className="pl-6 text-xs leading-relaxed text-fg-muted">{t("settings.highlightHint")}</p>
        </div>

        {!isIOS && (
          <div className="flex flex-col gap-1.5">
            <Checkbox checked={copyOnSelect} onChange={(copyOnSelect) => update({ copyOnSelect })}>
              <span className="font-medium text-fg">{t("settings.copyOnSelect")}</span>
            </Checkbox>
            <p className="pl-6 text-xs leading-relaxed text-fg-muted">{t("settings.copyOnSelectHint")}</p>
          </div>
        )}

        <div className="flex flex-col gap-1.5">
          <Checkbox checked={screenReader} onChange={(screenReader) => update({ screenReader })}>
            <span className="font-medium text-fg">{t("settings.screenReader")}</span>
          </Checkbox>
          <p className="pl-6 text-xs leading-relaxed text-fg-muted">{t("settings.screenReaderHint")}</p>
        </div>

        {/* Where the system can confirm the owner: Face ID / Touch ID or the device password. */}
        {(isIOS || isMac) && (
          <div className="flex flex-col gap-1.5">
            <Checkbox checked={appLock} onChange={(on) => void setAppLock(on)}>
              <span className="font-medium text-fg">{isIOS ? t("settings.appLockIos") : t("settings.appLockMac")}</span>
            </Checkbox>
            <p className="pl-6 text-xs leading-relaxed text-fg-muted">{t("settings.appLockHint")}</p>
            {lockError && <p className="pl-6 text-xs text-red">{lockError}</p>}
          </div>
        )}

        {canUpdate && <UpdatesSection />}

        <Section label={t("settingsAlerts.title")}>
          <Checkbox checked={alertsEnabled} onChange={(alertsEnabled) => update({ alertsEnabled })}>
            {t("settingsAlerts.enabled")}
          </Checkbox>
          {alertsEnabled && (
            <>
              <Checkbox checked={alertsNotify} onChange={(alertsNotify) => update({ alertsNotify })}>
                {t("settingsAlerts.notify")}
              </Checkbox>
              <div className="mt-1 grid grid-cols-3 gap-2">
                {(
                  [
                    ["alertDisk", alertDisk, t("settingsAlerts.disk")],
                    ["alertMemory", alertMemory, t("settingsAlerts.memory")],
                    ["alertCpu", alertCpu, t("settingsAlerts.cpu")],
                  ] as const
                ).map(([key, value, label]) => (
                  <label key={key} className="flex flex-col gap-1 rounded-card border border-border px-3 py-2">
                    <span className="text-xs text-fg-muted">
                      {label} · {t("settingsAlerts.threshold")}
                    </span>
                    <span className="flex items-center gap-1">
                      <select
                        value={value}
                        onChange={(e) => update({ [key]: Number(e.target.value) })}
                        className="cursor-pointer bg-transparent text-sm font-medium text-fg outline-none"
                      >
                        {[70, 75, 80, 85, 90, 95].map((n) => (
                          <option key={n} value={n}>
                            {n} %
                          </option>
                        ))}
                      </select>
                    </span>
                  </label>
                ))}
              </div>
            </>
          )}
        </Section>
      </div>
    </Modal>
  );
}

/** The version, automatic checks and a check by hand, with what it found. */
function UpdatesSection() {
  const { t } = useTranslation();
  const checkUpdates = useSettings((s) => s.checkUpdates);
  const update = useSettings((s) => s.update);
  const status = useUpdates((s) => s.status);
  const busy = status.kind === "checking" || status.kind === "downloading" || status.kind === "restart";
  return (
    <Section label={t("updates.title")}>
      <div className="flex items-center justify-between gap-3">
        <span className="text-sm text-fg-secondary">{t("updates.current", { version: APP_VERSION })}</span>
        <Button size="sm" disabled={busy} onClick={() => void useUpdates.getState().check()}>
          {status.kind === "checking" && <LoaderCircle className="size-3.5 animate-spin" />}
          {status.kind === "checking" ? t("updates.checking") : t("updates.checkNow")}
        </Button>
      </div>
      {status.kind === "upToDate" && <p className="text-xs text-green">{t("updates.upToDate")}</p>}
      {status.kind === "available" && (
        <div className="flex items-center justify-between gap-3 rounded-card border border-accent/40 bg-accent/10 px-3 py-2">
          <span className="text-sm font-medium text-fg">{t("updates.available", { version: status.version })}</span>
          <Button size="sm" variant="primary" onClick={() => void useUpdates.getState().install()}>
            {t("updates.install")}
          </Button>
        </div>
      )}
      {status.kind === "downloading" && <p className="text-xs text-fg-muted">{t("updates.downloading", { version: status.version })}</p>}
      {status.kind === "error" && <p className="break-words text-xs text-red">{status.message}</p>}
      <Checkbox checked={checkUpdates} onChange={(checkUpdates) => update({ checkUpdates })}>
        <span className="font-medium text-fg">{t("updates.auto")}</span>
      </Checkbox>
      <p className="pl-6 text-xs leading-relaxed text-fg-muted">{t("updates.autoHint")}</p>
    </Section>
  );
}

function Section({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-2">
      <span className="text-sm font-medium text-fg">{label}</span>
      {children}
    </div>
  );
}
