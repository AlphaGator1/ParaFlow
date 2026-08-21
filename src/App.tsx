import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import Setup, { type SetupStatus } from "./Setup";
import "./styles.css";

type Stage =
  | { kind: "idle" }
  | { kind: "waking" }
  | { kind: "listening" }
  | { kind: "transcribing" }
  | { kind: "polishing" }
  | { kind: "done"; text: string; latencyMs: number }
  | { kind: "error"; message: string };

type Mic = { open: boolean; device: string; closesInS: number };

const BAR_COUNT = 13;

export default function App() {
  const [stage, setStage] = useState<Stage>({ kind: "idle" });
  const [mic, setMic] = useState<Mic>({ open: false, device: "", closesInS: 0 });
  const [bars, setBars] = useState<number[]>(() => new Array(BAR_COUNT).fill(0));
  // undefined while we ask the backend; null once setup is not needed.
  const [setupStatus, setSetupStatus] = useState<SetupStatus | null | undefined>(undefined);
  // The live push-to-talk chord, so the idle hint never advertises stale keys.
  const [hotkey, setHotkey] = useState<string[]>(["Ctrl", "Win"]);
  const resetTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    invoke<SetupStatus>("setup_status")
      .then((s) => setSetupStatus(s.needsSetup ? s : null))
      // If the backend cannot be reached the HUD is the safer thing to show:
      // the pipeline may well be running fine.
      .catch(() => setSetupStatus(null));

    invoke<{ hotkey: string[] }>("get_settings")
      .then((s) => setHotkey(s.hotkey))
      .catch(() => {});

    const unDone = listen("paraflow://setup-done", () => setSetupStatus(null));
    const unSettings = listen<{ hotkey: string[] }>("paraflow://settings-changed", (e) =>
      setHotkey(e.payload.hotkey),
    );
    return () => {
      unDone.then((f) => f());
      unSettings.then((f) => f());
    };
  }, []);

  useEffect(() => {
    const unStage = listen<Stage>("flow://stage", (e) => {
      setStage(e.payload);

      // The "done" card is a confirmation, not a state to sit in - fade back
      // to idle so the HUD does not linger over the user's work.
      window.clearTimeout(resetTimer.current);
      if (e.payload.kind === "done" || e.payload.kind === "error") {
        resetTimer.current = window.setTimeout(
          () => setStage({ kind: "idle" }),
          e.payload.kind === "error" ? 5000 : 2600,
        );
      }
    });

    const unMic = listen<Mic>("flow://mic", (e) => setMic(e.payload));

    const unLevel = listen<{ peak: number }>("flow://level", (e) => {
      // Speech peaks are small and clumped near zero; a square root spreads
      // them across the bar range so the meter actually moves.
      const amp = Math.min(1, Math.sqrt(e.payload.peak) * 1.6);
      setBars((prev) => [...prev.slice(1), amp]);
    });

    return () => {
      unStage.then((f) => f());
      unMic.then((f) => f());
      unLevel.then((f) => f());
      window.clearTimeout(resetTimer.current);
    };
  }, []);

  // Render nothing until we know which face to show, so the HUD does not
  // flash up in front of the setup screen on first run.
  if (setupStatus === undefined) return null;
  if (setupStatus) return <Setup status={setupStatus} />;

  return (
    <div className="hud" data-stage={stage.kind}>
      <div className="pill">
        <div className="glyph">
          <Visual stage={stage} bars={bars} />
        </div>
        <div className="body">
          <Label stage={stage} mic={mic} hotkey={hotkey} />
        </div>
        <MicBadge mic={mic} />
      </div>
    </div>
  );
}

/// The honest indicator of whether the microphone is open. Deliberately the
/// most prominent thing on the HUD after the transcript itself.
function MicBadge({ mic }: { mic: Mic }) {
  return (
    <div className={`mic ${mic.open ? "on" : "off"}`} title={mic.device || "No microphone open"}>
      <span className="mic-dot" />
      <span className="mic-text">{mic.open ? "LIVE" : "OFF"}</span>
    </div>
  );
}

function Visual({ stage, bars }: { stage: Stage; bars: number[] }) {
  switch (stage.kind) {
    case "listening":
      return (
        <div className="wave">
          {bars.map((v, i) => (
            <span
              key={i}
              className="bar"
              style={{ height: `${8 + v * 30}px`, opacity: 0.45 + v * 0.55 }}
            />
          ))}
        </div>
      );
    case "waking":
    case "transcribing":
    case "polishing":
      return <div className="spinner" />;
    case "error":
      return <div className="dot error" />;
    case "done":
      return <div className="dot done" />;
    default:
      return <div className="dot idle" />;
  }
}

function Label({ stage, mic, hotkey }: { stage: Stage; mic: Mic; hotkey: string[] }) {
  switch (stage.kind) {
    case "idle":
      return (
        <>
          <div className="title">Ready</div>
          <div className="sub">
            {mic.open ? (
              <>Mic closes in {mic.closesInS}s</>
            ) : (
              <>
                Hold{" "}
                {hotkey.map((k, i) => (
                  <span key={k}>
                    {i > 0 && " + "}
                    <kbd>{k}</kbd>
                  </span>
                ))}{" "}
                to dictate
              </>
            )}
          </div>
        </>
      );
    case "waking":
      return (
        <>
          <div className="title">Opening mic…</div>
          <div className="sub">Speak after the chime</div>
        </>
      );
    case "listening":
      return (
        <>
          <div className="title">Listening…</div>
          <div className="sub">Release to transcribe</div>
        </>
      );
    case "transcribing":
      return (
        <>
          <div className="title">Transcribing…</div>
          <div className="sub">Running locally on your GPU</div>
        </>
      );
    case "polishing":
      return (
        <>
          <div className="title">Cleaning up…</div>
          <div className="sub">Punctuation, fillers, corrections</div>
        </>
      );
    case "done":
      return (
        <>
          <div className="title truncate">{stage.text}</div>
          <div className="sub">Pasted in {stage.latencyMs} ms</div>
        </>
      );
    case "error":
      return (
        <>
          <div className="title">Something went wrong</div>
          <div className="sub truncate">{stage.message}</div>
        </>
      );
  }
}
