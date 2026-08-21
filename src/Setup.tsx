import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useFitWindowToContent } from "./useFitWindow";

const WIDTH = 620;
const MIN_HEIGHT = 420;

type Accel = "cuda" | "vulkan";
type Profile = "transcribe" | "polish";

export type SetupStatus = {
  needsSetup: boolean;
  accel: Accel;
  transcribeMb: number;
  polishMb: number;
  /** What is left to fetch once files already on disk are discounted. */
  transcribeRemainingMb: number;
  polishRemainingMb: number;
};

type Progress = {
  label: string;
  index: number;
  total: number;
  bytes: number;
  ofBytes: number;
  done: boolean;
  /** Already on disk; nothing was transferred for this step. */
  skipped: boolean;
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
  useFitWindowToContent(pageRef, WIDTH, MIN_HEIGHT, [started, error]);

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

  const full = choice === "polish" ? status.polishMb : status.transcribeMb;
  const remaining =
    choice === "polish" ? status.polishRemainingMb : status.transcribeRemainingMb;
  const alreadyHave = Math.max(0, full - remaining);

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
          Install — {gb(remaining)}
        </button>
        <p className="footnote">
          {alreadyHave > 0 ? (
            <>
              {gb(alreadyHave)} is already on this computer and will be reused.{" "}
            </>
          ) : null}
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
            <div
              className={`progress-fill ${progress.skipped ? "skipped" : ""}`}
              style={{ width: progress.skipped ? "100%" : `${pct}%` }}
            />
          </div>
          <div className="bytes">
            {progress.skipped
              ? "Already on this computer — skipping"
              : progress.ofBytes > 0
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
