import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./styles.css";

type Overlay = "bottom" | "top" | "centre" | "hidden";

type Settings = {
  hotkey: string[];
  overlay: Overlay;
  idleSeconds: number;
  cleanup: boolean;
  chimes: boolean;
};

const OVERLAY_LABELS: Record<Overlay, string> = {
  bottom: "Bottom of screen",
  top: "Top of screen",
  centre: "Centre of screen",
  hidden: "Hidden",
};

export default function Settings() {
  const [s, setS] = useState<Settings | null>(null);
  const [capturing, setCapturing] = useState(false);
  const [preview, setPreview] = useState<string[]>([]);
  const [saved, setSaved] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const savedTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    invoke<Settings>("get_settings").then(setS).catch(() => {});

    const unPreview = listen<string[]>("paraflow://capture-preview", (e) => setPreview(e.payload));
    const unDone = listen<string[]>("paraflow://capture-done", (e) => {
      setCapturing(false);
      setPreview([]);
      setS((prev) => (prev ? { ...prev, hotkey: e.payload } : prev));
    });
    const unNotice = listen<string>("paraflow://notice", (e) => setNotice(e.payload));

    return () => {
      unPreview.then((f) => f());
      unDone.then((f) => f());
      unNotice.then((f) => f());
      window.clearTimeout(savedTimer.current);
      // Leaving with capture armed would swallow every keystroke system-wide.
      invoke("cancel_hotkey_capture").catch(() => {});
    };
  }, []);

  if (!s) return <div className="settings" />;

  function update<K extends keyof Settings>(key: K, value: Settings[K]) {
    setS((prev) => (prev ? { ...prev, [key]: value } : prev));
    setSaved(false);
  }

  async function save() {
    if (!s) return;
    try {
      await invoke("save_settings", { next: s });
      setSaved(true);
      window.clearTimeout(savedTimer.current);
      savedTimer.current = window.setTimeout(() => setSaved(false), 2400);
    } catch (e) {
      setNotice(String(e));
    }
  }

  async function beginCapture() {
    setCapturing(true);
    setPreview([]);
    setNotice(null);
    await invoke("start_hotkey_capture");
  }

  async function stopCapture() {
    setCapturing(false);
    setPreview([]);
    await invoke("cancel_hotkey_capture");
  }

  const shown = capturing && preview.length ? preview : s.hotkey;

  return (
    <div className="settings">
      <h1>ParaFlow Settings</h1>

      {/* --- hotkey --------------------------------------------------- */}
      <section>
        <h2>Push-to-talk key</h2>
        <p className="hint">
          Hold this to dictate. Pick something you would never press by accident while
          typing — combinations with Ctrl, Alt or Win work best.
        </p>

        <div className={`chord ${capturing ? "capturing" : ""}`}>
          {shown.length ? (
            shown.map((k, i) => (
              <span key={i}>
                <kbd>{k}</kbd>
                {i < shown.length - 1 && <span className="plus">+</span>}
              </span>
            ))
          ) : (
            <span className="chord-empty">Press a combination…</span>
          )}
        </div>

        {capturing ? (
          <>
            <p className="hint capturing-hint">
              Press and hold your combination, then release. All keys are being captured, so
              nothing else on your computer will react.
            </p>
            <button className="secondary" onClick={stopCapture}>
              Cancel
            </button>
          </>
        ) : (
          <button className="secondary" onClick={beginCapture}>
            Change…
          </button>
        )}
      </section>

      {/* --- overlay -------------------------------------------------- */}
      <section>
        <h2>Overlay</h2>
        <p className="hint">
          The small panel showing what ParaFlow is doing. It never takes focus and clicks pass
          straight through it, so it cannot be dragged — pick a corner instead.
        </p>
        <select value={s.overlay} onChange={(e) => update("overlay", e.target.value as Overlay)}>
          {(Object.keys(OVERLAY_LABELS) as Overlay[]).map((k) => (
            <option key={k} value={k}>
              {OVERLAY_LABELS[k]}
            </option>
          ))}
        </select>
      </section>

      {/* --- microphone ----------------------------------------------- */}
      <section>
        <h2>Microphone</h2>
        <p className="hint">
          The microphone opens when you first press the key and closes again after a quiet
          spell, so it is not listening while you are not using it.
        </p>
        <label className="row">
          <span>Close after</span>
          <input
            type="number"
            min={5}
            max={3600}
            value={s.idleSeconds}
            onChange={(e) => update("idleSeconds", Number(e.target.value))}
          />
          <span>seconds of no dictation</span>
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={s.chimes}
            onChange={(e) => update("chimes", e.target.checked)}
          />
          <span>
            Play a chime when the microphone opens and closes
            <em>This is how you can hear that it stopped listening.</em>
          </span>
        </label>
      </section>

      {/* --- cleanup -------------------------------------------------- */}
      <section>
        <h2>Cleanup</h2>
        <label className="check">
          <input
            type="checkbox"
            checked={s.cleanup}
            onChange={(e) => update("cleanup", e.target.checked)}
          />
          <span>
            Tidy up what I say before typing it
            <em>
              Removes “um” and false starts, applies corrections you speak aloud, and matches
              the app you are typing into. Adds about a tenth of a second.
            </em>
          </span>
        </label>
        <button className="secondary" onClick={() => invoke("open_dictionary_file")}>
          Edit personal dictionary…
        </button>
        <p className="hint">
          A list of names and terms ParaFlow should spell your way. Opens in your text editor;
          restart ParaFlow to apply changes.
        </p>
      </section>

      {notice && <p className="notice">{notice}</p>}

      <footer>
        <button className="primary" onClick={save}>
          {saved ? "Saved" : "Save settings"}
        </button>
      </footer>
    </div>
  );
}
