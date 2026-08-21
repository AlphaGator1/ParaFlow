import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow, currentMonitor } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";

const WIDTH = 620;
/// Never shrink below this, however little content there is.
const MIN_HEIGHT = 420;
/// Leave room for the taskbar and a little breathing space.
const SCREEN_FRACTION = 0.9;

/**
 * Grow the window to fit its content, up to what the monitor can show.
 *
 * The alternative — a fixed height — either scrolls on a normal display or
 * overflows a small one, since the two option cards are long by design. This
 * measures what the content actually needs and asks for exactly that, falling
 * back to scrolling only when the screen genuinely cannot fit it.
 */
function useFitWindowToContent(ref: React.RefObject<HTMLDivElement>, deps: unknown[]) {
  useLayoutEffect(() => {
    let cancelled = false;
    // Two frames: one for React to commit, one for the browser to lay out and
    // settle webfont metrics. Measuring earlier reports a too-short page.
    const raf = requestAnimationFrame(() =>
      requestAnimationFrame(async () => {
        const el = ref.current;
        if (!el || cancelled) return;
        try {
          const monitor = await currentMonitor();
          const scale = monitor?.scaleFactor ?? 1;
          const maxHeight = monitor
            ? (monitor.size.height / scale) * SCREEN_FRACTION
            : Number.MAX_SAFE_INTEGER;

          const needed = el.scrollHeight;
          const height = Math.round(Math.min(Math.max(needed, MIN_HEIGHT), maxHeight));

          const win = getCurrentWindow();
          await win.setSize(new LogicalSize(WIDTH, height));
          await win.center();
        } catch {
          // Window sizing is a nicety; the page scrolls perfectly well without
          // it, so a failure here must not break setup.
        }
      }),
    );
    return () => {
      cancelled = true;
      cancelAnimationFrame(raf);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}

type Accel = "cuda" | "vulkan";
type Profile = "transcribe" | "polish";

export type SetupStatus = {
  needsSetup: boolean;
  accel: Accel;
  transcribeMb: number;
  polishMb: number;
};

type Progress = {
  label: string;
  index: number;
  total: number;
  bytes: number;
  ofBytes: number;
  done: boolean;
};

function gb(mb: number) {
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb} MB`;
}

export default function Setup({ status }: { status: SetupStatus }) {
  const [choice, setChoice] = useState<Profile>("polish");
  const [started, setStarted] = useState(false);
  const [progress, setProgress] = useState<Progress | null>(null);
  const [error, setError] = useState<string | null>(null);
  const pageRef = useRef<HTMLDivElement>(null);

  // Re-fit when the error banner appears or the choice changes the button text.
  useFitWindowToContent(pageRef, [started, error]);

  useEffect(() => {
    const unProg = listen<Progress>("paraflow://setup-progress", (e) => setProgress(e.payload));
    const unFail = listen<string>("paraflow://setup-failed", (e) => {
      setError(e.payload);
      setStarted(false);
    });
    return () => {
      unProg.then((f) => f());
      unFail.then((f) => f());
    };
  }, []);

  async function begin() {
    setError(null);
    setStarted(true);
    try {
      await invoke("begin_setup", { profile: choice });
    } catch (e) {
      setError(String(e));
      setStarted(false);
    }
  }

  if (started) {
    return <Downloading progress={progress} error={error} pageRef={pageRef} />;
  }

  return (
    <div className="setup" ref={pageRef}>
      <header>
        <h1>Set up ParaFlow</h1>
        <p className="lede">
          ParaFlow types what you say, anywhere on your computer. Everything runs on this
          machine — your voice is never sent anywhere.
        </p>
      </header>

      <Hardware accel={status.accel} />

      <h2>What should ParaFlow install?</h2>

      <Option
        id="polish"
        checked={choice === "polish"}
        onSelect={setChoice}
        title="Dictation with cleanup"
        badge="Recommended"
        size={gb(status.polishMb)}
        summary="Turns what you say into clean, written text."
        bullets={[
          "Removes “um”, “uh” and false starts",
          "Applies corrections you speak — say “Tuesday, no, Wednesday” and only Wednesday is typed",
          "Turns spoken lists into real bulleted lists",
          "Matches where you're typing: a command in a terminal, full sentences in an email",
        ]}
        cost="Adds roughly a tenth of a second per sentence. You can switch it off later without reinstalling."
      />

      <Option
        id="transcribe"
        checked={choice === "transcribe"}
        onSelect={setChoice}
        title="Dictation only"
        size={gb(status.transcribeMb)}
        summary="Types out exactly what you said, with punctuation and capitalisation."
        bullets={[
          "Fastest option, and much smaller to download",
          "“Um” and “uh” get typed out too",
          "Spoken corrections are typed literally — you'd get “Tuesday, no, Wednesday”",
          "No lists, and no adapting to the app you're in",
        ]}
        cost="Choose this if disk space or download size matters most. You can add cleanup later."
      />

      {error && <p className="error">{error}</p>}

      <footer>
        <button className="primary" onClick={begin}>
          Install — {gb(choice === "polish" ? status.polishMb : status.transcribeMb)}
        </button>
        <p className="footnote">
          Downloads to your user folder. You can close and reopen ParaFlow mid-download; it
          picks up where it left off.
        </p>
      </footer>
    </div>
  );
}

function Hardware({ accel }: { accel: Accel }) {
  return (
    <div className={`hardware ${accel}`}>
      <strong>{accel === "cuda" ? "NVIDIA graphics card found" : "Using standard graphics support"}</strong>
      <span>
        {accel === "cuda"
          ? "ParaFlow will use it, which makes both dictation and cleanup fast."
          : "Works with AMD and Intel graphics. Dictation is fast; the cleanup step is noticeably slower than on an NVIDIA card, but still works."}
      </span>
    </div>
  );
}

function Option(props: {
  id: Profile;
  checked: boolean;
  onSelect: (p: Profile) => void;
  title: string;
  badge?: string;
  size: string;
  summary: string;
  bullets: string[];
  cost: string;
}) {
  return (
    <label className={`option ${props.checked ? "checked" : ""}`}>
      <input
        type="radio"
        name="profile"
        checked={props.checked}
        onChange={() => props.onSelect(props.id)}
      />
      <div className="option-body">
        <div className="option-head">
          <span className="option-title">{props.title}</span>
          {props.badge && <span className="badge">{props.badge}</span>}
          <span className="size">{props.size}</span>
        </div>
        <p className="summary">{props.summary}</p>
        <ul>
          {props.bullets.map((b, i) => (
            <li key={i}>{b}</li>
          ))}
        </ul>
        <p className="cost">{props.cost}</p>
      </div>
    </label>
  );
}

function Downloading({
  progress,
  error,
  pageRef,
}: {
  progress: Progress | null;
  error: string | null;
  pageRef: React.RefObject<HTMLDivElement>;
}) {
  const pct =
    progress && progress.ofBytes > 0
      ? Math.min(100, (progress.bytes / progress.ofBytes) * 100)
      : 0;

  return (
    <div className="setup downloading" ref={pageRef}>
      <header>
        <h1>Setting up…</h1>
        <p className="lede">
          This is a large download and only happens once. You can leave it running.
        </p>
      </header>

      {progress ? (
        <>
          <div className="step">
            Step {progress.index + 1} of {progress.total} — {progress.label}
          </div>
          <div className="progress-track">
            <div className="progress-fill" style={{ width: `${pct}%` }} />
          </div>
          <div className="bytes">
            {progress.ofBytes > 0
              ? `${(progress.bytes / 1048576).toFixed(0)} of ${(progress.ofBytes / 1048576).toFixed(0)} MB`
              : "Unpacking…"}
          </div>
        </>
      ) : (
        <div className="step">Starting…</div>
      )}

      {error && <p className="error">{error}</p>}
    </div>
  );
}
