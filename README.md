# ParaFlow

Push-to-talk dictation for Windows. Hold a key, speak, release — cleaned-up text
lands at your cursor in whatever app has focus. Everything runs on your own GPU;
no audio leaves the machine.

A local-first take on [Wispr Flow](https://wisprflow.ai), which does the same job
over the network for a subscription.

Built for Windows 11 with an NVIDIA GPU. Works on AMD and Intel too, with a
slower cleanup step.

---

## Install

Download the installer from [Releases](../../releases) and run it. It is small;
ParaFlow fetches the speech models it needs on first launch and tells you how
big that is before starting.

**Windows will warn you.** ParaFlow is not code-signed, so SmartScreen shows
"Windows protected your PC" — choose *More info* → *Run anyway*. If that is not
a trade you want to make, build it yourself: see *Building the installer*.

Once installed, hold `Ctrl`+`Win`, speak, and let go. The full guide is in
[docs/user-guide.html](docs/user-guide.html), and ships with the app.

---

## Status

Working and in daily use. Everything below is real and measured on an RTX 5090;
what is *not* done is in [NEXT.md](NEXT.md), which is deliberately candid about
it.

| | |
|---|---|
| ✅ Push-to-talk hold detection | `WH_KEYBOARD_LL` — press *and* release |
| ✅ Always-warm mic + 300 ms pre-roll | no device-open latency, no clipped first syllable |
| ✅ Local GPU transcription | whisper.cpp + CUDA, model resident in VRAM |
| ✅ Clipboard-preserving paste | works in Electron, terminals, browsers, native |
| ✅ Live HUD | level meter, stage, measured latency, mic state |
| ✅ **Full round trip verified** | 160 ms from key release to pasted text |
| ✅ LLM cleanup pass | fillers, punctuation, self-corrections, lists |
| ✅ On-demand microphone | opens on first press, closes when the burst ends |
| ✅ Personal dictionary | corrects mishearings to your own spellings |
| ✅ Per-app tone | a terminal is not an email |
| ✅ Hands-free latch | double-tap to lock on, single press to stop |
| ✅ Installer + user guide | 2.8 MB NSIS, everything else on first run |

### CUDA or Vulkan

Both runtimes ship a Vulkan build, which needs no cuBLAS and runs on AMD and
Intel GPUs as well as NVIDIA. Measured on an RTX 5090:

| stage | CUDA | Vulkan | runtime on disk |
|---|---|---|---|
| ASR (Parakeet 0.6B) | 47 ms | **55 ms** | 1093 MB → **113 MB** |
| Cleanup (Qwen3 4B) | 66 ms | **588 ms** | 670 MB → **97 MB** |

**Vulkan is fine for transcription and unusable for cleanup.** 17% slower on
the small encoder model, 9x slower on autoregressive token generation — which
matches llama.cpp's known CUDA/Vulkan gap. It is genuinely on the GPU in both
cases; VRAM went from 1365 MB to 4563 MB during the Vulkan cleanup run, so this
is not a silent CPU fallback.

Vulkan's *first ever* run on a machine costs ~4.6 s compiling shaders. The
driver caches them, so later launches are 85–93 ms cold and 55 ms warm — faster
cold than CUDA's 151 ms. `Asr::warm_up` absorbs it either way.

### First-run setup

Nothing large ships in the installer. On first launch ParaFlow shows a setup
screen, detects the GPU, and downloads what the chosen configuration needs into
`%LOCALAPPDATA%\ParaFlow\`.

The user is asked **one** question — transcription only, or transcription plus
cleanup:

| choice | download (NVIDIA) | what it does |
|---|---|---|
| **Dictation with cleanup** *(default)* | ~4.6 GB | removes fillers, applies spoken corrections, formats lists, matches the target app |
| Dictation only | ~1.7 GB | types what you said, with punctuation and casing |

Everything else is decided rather than asked. The GPU backend comes from
looking for `nvcuda.dll` — installed by the NVIDIA *driver*, so its presence
means CUDA will actually run — because the alternative is asking a
non-technical user to identify their own graphics card. On anything else the
plan switches to Vulkan, and the screen says plainly that cleanup will be
slower.

The downloader resumes: it writes to a `.part` file, sends a `Range` header if
one is there, and falls back to starting over if the server ignores it and
replies 200 with the whole file. It verifies against `Content-Length` and fails
loudly rather than leaving a truncated model on disk. Assets already present
are skipped, so re-running setup is cheap.

`PARAFLOW_FORCE_SETUP=1` shows the screen on a machine that is already set up,
which is the only way to look at it after the first run. It deletes nothing.

### Building the installer

```powershell
npm run tauri build
# -> src-tauri/target/release/bundle/nsis/ParaFlow_0.1.1_x64-setup.exe
```

**2.8 MB.** Everything else arrives on first run.

`installMode` is `perMachine` deliberately. Tauri's `currentUser` mode installs
to `$LOCALAPPDATA\<productName>` — byte-for-byte the directory this app keeps
models, runtimes and the dictionary in. That is not destructive (the
uninstaller uses a non-recursive `RMDir`, which a populated `models/` blocks)
but it leaves `paraflow.exe` sitting among gigabytes of weights, and an
uninstall leaves an orphaned data folder with no app in it. `perMachine`
installs to `Program Files` and keeps the two apart, at the cost of one
elevation prompt.

Data survives an uninstall by default, so reinstalling does not re-download
several gigabytes.

**The "delete application data" checkbox used to be a lie.** Tauri's template
honours it by removing `$LOCALAPPDATA\<bundle id>` - for us
`com.paraflow.dictation`, a directory that has never existed - while everything
real sits in `$LOCALAPPDATA\ParaFlow`. Ticking the box removed nothing and
reported success. `NSIS_HOOK_POSTUNINSTALL` now handles it, and because this is
an irreversible multi-gigabyte delete it asks first, naming the directory and
listing what goes with it (models, settings, dictionary). The dialog defaults to
*No*.

To remove the data by hand instead:

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\ParaFlow"
```

### Packaging

`tauri.conf.json` bundles exactly one resource, the ~30 KB user guide.
`vendor/` and `models/` are deliberately **not** bundled: doing so copied
2.8 GB into `target/debug` on every dev run, and weights barely compress —
GGUF is already quantised, so LZMA gets it to 95% of original. Anything
bundled is paid for almost at full size.

Measured compression: **weights 95%**, DLLs 70%, executables 92%. That makes
what you bundle matter far more than how you compress it:

| installer contents | size |
|---|---|
| Parakeet + whisper fallback, weights bundled, CUDA | ~6.8 GB |
| Parakeet only, weights bundled, CUDA | ~4.5 GB |
| Parakeet only, weights on first run, CUDA | ~1.44 GB |
| Parakeet only, weights on first run, Vulkan | **~79 MB** |

The intended shape: ship the app plus runtimes, and pull weights on first
launch into `%LOCALAPPDATA%\ParaFlow\models\` with progress in the HUD —
resumable and checksum-verified, which is why it belongs in Rust rather than
in an NSIS download plugin.

The whisper fallback costs ~2.3 GB (1472 MB model + 851 MB runtime) and should
be an optional component rather than a default: it exists for decoder prompt
biasing, which Parakeet lacks, and as an escape hatch if Parakeet mishears a
particular voice.

Until that exists, `resolve()` in `lib.rs` walks up from the executable and
finds the project-root `vendor/` and `models/` directories, so development
works as-is and a built app must be run from the repo.

### User guide

`docs/user-guide.html` is the end-user documentation: how dictation works, what
cleanup does, when the microphone is open, and what to try when nothing gets
typed. It is a single self-contained HTML file with no external assets, so it
works with the network off — which matters for an app whose whole pitch is that
it works with the network off.

HTML rather than Markdown for one blunt reason: on Windows a `.md` file opens
in Notepad as raw `#` characters. An `.html` file double-clicks into a formatted
page the reader can search and print.

It is bundled as a Tauri resource and lands beside the executable as
`ParaFlow User Guide.html`. Four ways in, because a document nothing links to
is a document nobody reads:

- **Tray → User guide**
- **Settings → Help → Open the user guide**
- **Start menu**, via `src-tauri/nsis/hooks.nsh`, which adds a shortcut next to
  ParaFlow's own in `NSIS_HOOK_POSTINSTALL` and removes it again in
  `NSIS_HOOK_POSTUNINSTALL`. The hook honours `$UpdateMode` and
  `$NoShortcutMode` so it cannot resurrect a shortcut the user declined.
- **Automatically, once**, when first-run setup finishes — the one moment
  someone has the most questions and the least idea that a tray icon is where
  the answers live.

`guide_path()` prefers the resource directory and falls back to `resolve()`,
so a developer running from the repo gets `docs/user-guide.html` without a
build step.

## Requirements

- Windows 10/11 x64
- NVIDIA GPU (optional — falls back to CPU, which is fine for short utterances
  on a many-core part)
- Node 20+, Rust 1.77+, and MSVC build tools + Windows SDK

## Setup

```powershell
npm install
./scripts/setup.ps1
npm run tauri dev
```

`setup.ps1` fetches the inference runtime and model weights (~2.5 GB). They are
not in version control.

`setup.ps1 -NoCleanup` skips the cleanup model if you only want raw dictation.

### Settings

Tray → **Settings…**, or edit `%LOCALAPPDATA%\ParaFlow\settings.json` by hand:

```json
{
  "hotkey": ["Ctrl", "Win"],
  "overlay": "bottom",
  "idleSeconds": 30,
  "cleanup": true,
  "chimes": true,
  "handsFree": true
}
```

Chords are stored by **name**, not virtual-key number, so the file stays
readable and hand-editable. Unrecognised names fall back to the default rather
than producing an unpressable hotkey, and `idleSeconds` is clamped to 5–3600.
Every field is `#[serde(default)]`, so a partial file is valid.

Changing the hotkey, overlay, idle timeout, chimes or the hands-free latch
takes effect immediately. Turning cleanup on or off starts or stops a sidecar
holding gigabytes of VRAM, so that one asks for a restart.

The hotkey and the latch both live in two places - the settings store and the
hook's own state - so `save_settings` pushes each into the hook rather than
letting the hook read settings on the keystroke path.

**Rebinding** uses the low-level hook in a capture mode: it reads the chord and
swallows every keystroke while doing so, since the user is pressing arbitrary
combinations and having the capture field also open the Start menu would be its
own bug. It records the *largest* set of keys held at once rather than the
final state, because releasing a chord is gradual and reading the last key up
would capture one key.

### Configuration

Environment variables override the settings file where set, so a report can be
reproduced without editing someone's saved configuration. All optional:

| variable | default | meaning |
|---|---|---|
| `PARAFLOW_ASR` | `parakeet` | `parakeet` or `whisper` |
| `PARAFLOW_MODEL` | per backend | transcription model in `models/`, overrides the backend default |
| `PARAFLOW_CLEANUP_MODEL` | `Qwen3-4B-Instruct-2507-Q4_K_M.gguf` | cleanup model |
| `PARAFLOW_NO_CLEANUP` | unset | set to anything to paste raw transcripts |
| `PARAFLOW_IDLE_SECS` | `30` | idle time before the mic is released |
| `PARAFLOW_LOG` | `paraflow_lib=debug,info` | tracing filter |

```powershell
$env:PARAFLOW_IDLE_SECS = "60"; npm run tauri dev -- --release

# A/B the two transcription backends on your own speech:
$env:PARAFLOW_ASR = "whisper"; npm run tauri dev -- --release
```

---

## Architecture

```
  ┌────────────────────────────────────────────────────────────────┐
  │  hotkey thread          WH_KEYBOARD_LL hook + message pump     │
  │                         forwards PttEvent::{Down,Up} only      │
  └───────────────────────────────┬────────────────────────────────┘
                                  │  crossbeam channel
  ┌───────────────────────────────▼────────────────────────────────┐
  │  pipeline thread                                               │
  │                                                                │
  │   MicSession ────── opens on the first press of a burst,       │
  │        │            closes after 30 s idle. 120 s ring buffer;  │
  │        │            mark() reaches back 300 ms for pre-roll     │
  │        ▼                                                       │
  │   resample ──────── rubato FFT, device rate → 16 kHz mono      │
  │        ▼                                                       │
  │   noise gate ────── discard buffers under the noise floor      │
  │        ▼                                                       │
  │   Asr ───────────── HTTP → whisper-server    (1.6 GB VRAM)     │
  │        ▼                                                       │
  │   Polish ────────── HTTP → llama-server      (2.8 GB VRAM)     │
  │        │            fillers, punctuation, self-corrections;     │
  │        │            falls back to the raw transcript always     │
  │        ▼                                                       │
  │   inject ────────── save clipboard → set → Ctrl+V → restore    │
  └───────────────────────────────┬────────────────────────────────┘
                                  │  Tauri events
  ┌───────────────────────────────▼────────────────────────────────┐
  │  HUD (React)            transparent, always-on-top, never       │
  │                         focused — focus would eat the paste     │
  │                         LIVE/OFF badge reflects the mic         │
  └────────────────────────────────────────────────────────────────┘
```

### Sessions: when the microphone is actually on

An always-open microphone is the lowest-latency design and the wrong one for a
tool used in bursts — it leaves the capture light on all day. So the device is
opened lazily and released again:

```
  closed ──first press──► opening ──chime──► LIVE ──30 s idle──► closed
                           ~150 ms            ▲   │
                                              └───┘
                                          further presses:
                                        instant, with pre-roll
```

Only the first press of a burst pays the device-open cost, and a rising chime
marks the moment it is safe to speak (a falling one marks the mic being
released). Recording starts *after* the chime so the beep never reaches the
recogniser. `PARAFLOW_IDLE_SECS` tunes the timeout.

The OS microphone indicator is therefore an honest signal, and the HUD's
`LIVE`/`OFF` badge mirrors it.

### Hands-free latch

Double-tap the chord and recording locks on until the next press, so a
paragraph does not mean holding a key for two minutes. Off with
`PARAFLOW_NO_LATCH`, or the settings toggle.

The double-press window is `GetDoubleClickTime()` clamped to 200–600 ms, rather
than a constant. Someone who lengthened it in Ease of Access has told Windows
they need longer, and a latch that will not arm for them is a latch they cannot
use. The clamp exists because the setting is legally 100 ms (unhittable with a
two-key chord) up to several seconds (two separate dictations would latch by
accident).

**The hook has to know the pipeline is latched.** It is a shared flag, not
inference, and it earns its keep on one specific sequence: someone who latched
with a double-tap will often try to *stop* with one. Without the flag, tap one
stops the latch, its release arms the detector, and tap two immediately latches
again — the exact "my microphone stayed on" surprise the feature is supposed to
avoid. With it, any press while latched is unconditionally a stop and its
release is suppressed from arming. The pipeline clears the flag *before*
transcribing rather than after, so a press arriving during those ~120 ms starts
the next dictation instead of being swallowed.

**It stops itself at two minutes.** Not a taste decision: the capture ring holds
`RING_SECONDS` of audio and `Ring::slice` clamps to whatever is still resident
instead of erroring, so a latch that outran the ring would silently lose its own
beginning. `MAX_LATCHED` sits 30 s inside it, with a test asserting the headroom
and a countdown on the overlay for the last 20 s. It doubles as the answer to a
microphone left recording because somebody walked away.

Long recordings also broke two budgets in the cleanup pass that had been sized
for one-sentence bursts — see *Cleanup pass*.

### Why these choices

**Not `RegisterHotKey`.** Tauri's `global-shortcut` plugin wraps it, and it only
reports a *press*. Push-to-talk needs the release too, so the app installs a
low-level keyboard hook and watches raw transitions. The hook thread does nothing
but forward events — Windows silently uninstalls hooks that block for too long.

**The mic is never closed.** Opening a capture device costs 100–300 ms, which
would land in the middle of the latency budget. The stream opens once at startup
and every sample flows into a ring buffer, so starting a recording is just
noting an offset. This also gives pre-roll for free: `mark()` reaches back 300 ms,
so the syllable that beat your keystroke is already captured.

**The model never unloads.** Cold-loading `large-v3-turbo` takes ~840 ms. Paying
that per utterance would dominate everything else, so whisper.cpp runs as a
resident sidecar with the model in VRAM. It is assigned to a Windows job object,
so it cannot outlive the app and strand 1.6 GB of VRAM.

**Clipboard paste, not synthetic keystrokes.** Typing text character by character
drops characters in apps that throttle input, breaks IME state, and is slow for
long passages. Clipboard + `Ctrl`+`V` is atomic and works everywhere. The
previous clipboard contents are restored afterwards.

**Modifiers are explicitly released before pasting.** The app suppresses the Win
key-up to stop the Start menu from opening, so the target app can still believe
Win is held. Sending `Ctrl`+`V` on top of that would fire `Win`+`Ctrl`+`V`.

**The HUD never takes focus.** It is configured `focus: false` and
`pointer-events: none`. A window that steals focus would receive its own paste.

**The cleanup pass fails open.** Every failure path — sidecar down, timeout,
runaway output — returns the raw transcript. A stage that can lose the user's
words is worse than no stage at all, so `Polish::apply` cannot return an error.

### Making the cleanup model transcribe rather than answer

The hard part of the cleanup pass is not punctuation, it is stopping a helpful
model from *doing what the user said* instead of writing it down. Dictating
"write a Python function that reverses a string" must produce that sentence.

Measured on Qwen3-4B with exactly that transcript:

| prompt shape | result |
|---|---|
| few-shot examples + bare input | ❌ wrote the function |
| few-shot examples + `<transcript>`-delimited input | ✅ transcribed it |
| delimited input, no few-shot examples | ❌ wrote the function |

**Neither device is sufficient alone.** A bare user turn reads as an
instruction however many counter-examples precede it; delimiters alone lack the
demonstration of what "clean this up" means. Both are load-bearing, which is
why `wrap()` and `FEW_SHOT` carry warnings not to remove them.

Two independent guards catch it if the model does it anyway: runaway output
length, and *vocabulary drift* — cleanup only deletes fillers and adds
punctuation, so the output reuses nearly all the input's words, while a reply
reuses almost none. The drift check is the one that matters: the failure above
produced `def reverse_string(s): return s[::-1]`, almost exactly the same
length as the input, so a length check alone sailed straight past it.

### Personal dictionary

`%LOCALAPPDATA%\ParaFlow\dictionary.txt`, one name or term per line, created
with a comment header on first run. Terms are appended to the cleanup system
prompt, and the model corrects mishearings to the exact spelling written there:

```
in   Can you ask Prea to redeploy the kuber netes cluster?
out  Can you ask Priya to redeploy the Kubernetes cluster?
```

This started as a plan to use whisper-server's `--prompt` for decoder-level
biasing. Parakeet has no equivalent, so it moved into the cleanup prompt —
which turned out better: biasing only nudges words the decoder half-heard,
while an instruction model sees the whole sentence and can split "kuber netes".
It also works whichever ASR backend is selected.

The obvious failure mode is the model sprinkling dictionary terms into
sentences that never contained them, so the prompt says not to and
`examples/cleanup.rs` has a case that checks.

Dictionary terms live in the *system* message because they are fixed for the
process lifetime and so stay inside llama.cpp's cached prefix. Anything varying
per utterance has to go in the final turn instead — which is exactly where the
per-app style tag goes.

### Per-app tone

The target app is identified by **process name**, not window title — titles
change with the open document, get localised, and collide across apps.

| style | apps | effect |
|---|---|---|
| `Code` | editors, IDEs, terminals | spoken symbols become literal (`dash b` → `-b`), no trailing stop on commands |
| `Mail` | Outlook, Thunderbird | complete sentences, full punctuation |
| `Chat` | Slack, Discord, Teams | conversational, contractions kept |
| `Prose` | everything else | default |

Browsers deliberately map to `Prose`: a browser could be mail, chat, or an
editor, and guessing wrong is worse than not guessing.

`Code` splits on what was *dictated*, not on the app, because an editor holds
shell commands and prose comments alike. An earlier version of the hint said
only "terse" and stripped the capital and full stop off ordinary sentences.
Both behaviours are pinned by few-shot examples — the style hint alone was not
enough to teach either.

### Measured on the development machine

Ryzen 9 9950X3D · RTX 5090 (32 GB, `sm_120`) · Windows 11 · `large-v3-turbo`

Full transcription of an 11.0 s speech sample, over the real HTTP path
(`scripts/test-asr.ps1`):

```
  run 1     221 ms   cold — driver JIT
  run 2      91 ms
  run 3      90 ms
  run 4      90 ms
  ────────────────
  warm mean  90 ms   122x realtime

  sidecar ready   1212 ms   once, at startup
  VRAM resident   1624 MB   5% of the card
```

A typical dictated sentence is 3–5 s, so transcription lands well under the
~500 ms budget where push-to-talk stops feeling like a round trip and starts
feeling like typing.

End-to-end on live speech, release build, Parakeet backend
(18.65 s utterance, 48 kHz stereo webcam mic):

```
  key release  →  capture + resample to 16 kHz      4.5 ms    2%
               →  GPU transcription                92.1 ms   32%
               →  LLM cleanup                     195.0 ms   67%
               ─────────────────────────────────────────
                  release to pasted text          291.6 ms
```

Note the shape: **cleanup is now two thirds of the budget** and transcription
is no longer the bottleneck. Any further latency work belongs there — a smaller
instruction model, or skipping cleanup entirely for very short utterances where
there is little to clean.

### Run the release build

`npm run tauri dev` builds the Rust side unoptimized, and the resampler is
pathologically sensitive to it:

```
  resample 10.69 s of audio    debug   169.8 ms
                               release   2.0 ms      86x
```

Transcription and cleanup are unaffected — those run inside the sidecar
processes, which are optimized binaries either way — so a debug build shows a
realistic GPU cost sitting behind an unrealistic capture cost.

This was worth checking before optimizing: the resampler looked like the most
expensive stage in the pipeline and is in fact ~1% of it.

**To actually use the app**, run it standalone rather than through
`tauri dev` — no vite server, and no file watcher:

```powershell
npm run build
cd src-tauri; cargo run --release --features custom-protocol
```

`--features custom-protocol` is not optional. It is what switches the webview
from `devUrl` to the assets embedded in the binary; without it the window comes
up showing `ERR_CONNECTION_REFUSED` against `http://localhost:1420`, because
the pipeline starts fine and only the UI is looking for a dev server that is
not there. `tauri build` passes it for you; a bare `cargo run --release` does
not.

`npm run tauri dev -- --release` is for iterating on the UI. Be aware its
watcher rebuilds and **relaunches** on every source edit, so editing Rust while
it runs leaves you with several live instances competing for the sidecar ports.
Startup now refuses to continue if a port is already held, rather than silently
attaching to another instance's model.

### Choosing the transcription backend

Two backends, selected by `PARAFLOW_ASR`. Both are resident HTTP sidecars
taking a WAV over multipart, so the client code in `asr.rs` is shared and only
the flags and endpoint differ.

Same 11.0 s sample, same harness, warm:

| backend | size | warm | load | notes |
|---|---|---|---|---|
| **Parakeet TDT 0.6B v3 q8_0** *(default)* | **897 MB** | **45 ms** | 526 ms | punctuation + casing native |
| Parakeet v3 f16 | 1374 MB | 45 ms | 526 ms | identical output to q8_0 |
| whisper large-v3-turbo | 1549 MB | 99 ms | 1195 ms | supports decoder prompt biasing |
| whisper small.en | 465 MB | 110 ms | — | *slower*, no accuracy gain |
| whisper base.en | 141 MB | 53 ms | — | drops commas; cleanup restores them |

Parakeet is **2.2× faster and 42% smaller** than the whisper default, and
produced a byte-identical transcript on this sample, punctuation included.
Quantizing to q8_0 cost nothing measurable against f16 on either axis, so the
smaller file is the default.

Confirmed on live dictation, not just the benchmark clip: an 18.65 s utterance
transcribed in 92 ms, which normalises to **4.9 ms per second of audio against
whisper's 8.2** — about 1.7x faster on real speech. Transcript quality was
reported as indistinguishable, proper nouns included.

One caveat remains:

- **Parakeet has no prompt biasing.** Its only knobs are `--decoder`, `--lang`,
  and `--stream`. whisper-server's `--prompt` can bias decoding toward supplied
  vocabulary; Parakeet has no equivalent, which changes how a personal
  dictionary has to be built (see `NEXT.md`). This is the main reason whisper
  stays selectable rather than being deleted.

Among the whisper models, the counterintuitive result is that **an English-only
model does not reduce latency** — `small.en` is slower than the multilingual
`large-v3-turbo`, which is distilled to 4 decoder layers where `small` has 12.
English-only operation comes from pinning `-l en` to skip language detection,
not from the model choice.

### Cleanup pass

Qwen3-4B-Instruct Q4\_K\_M, 2.8 GB VRAM, over 13 cases in `examples/cleanup.rs`:

```
  mean 197 ms  including one two-minute case; ~65 ms without it
```

Cost scales with *output* length, not input: a 210-character result took 195 ms
against 65 ms for the short harness cases. That makes cleanup the dominant
stage, and the only remaining latency lever — a smaller instruction model would
roughly halve it at some cost in cleanup quality. Not currently worth trading,
since the total sits comfortably under the ~500 ms threshold where dictation
starts to feel like a round trip.

**Two budgets here were sized for one-sentence bursts and the hands-free latch
outgrew them.** Both failed in the direction that is hardest to notice:

- `max_tokens` was capped at 512. The harness's two-minute case cleans to 2351
  characters, which fits inside 512 with essentially nothing spare — a ceiling
  with no headroom is not a margin. Worse, hitting it *truncates* rather than
  fails: `looks_like_an_answer` only catches output that is too **long**, so a
  cut-off transcript would have been pasted as though it were complete. The cap
  is now 1536, and a `finish_reason == "length"` check rejects a truncated
  response outright so `apply` falls back to the full raw transcript.
  Verified by forcing the ceiling to 128: the guard fires and 2382 characters
  come back unchanged rather than cut short.
- The request timeout was a flat 2500 ms. Right when every utterance was a
  sentence — waiting three seconds to punctuate one line is worse than the raw
  transcript — and wrong afterwards, because after two minutes of dictation a
  few extra seconds is cheap and losing cleanup entirely is not. It now scales
  with expected output, capped at 8 s. The two-minute case takes 1.78 s.

Run the harness after any prompt, model, or style change. Three cases are
adversarial — two catch the model answering instead of transcribing, one
catches it inserting dictionary terms that were never spoken — and the
two-minute case is what the latch actually produces:

```powershell
cargo run --release --example cleanup
```

### A note on Blackwell

The published whisper.cpp CUDA binaries target CUDA 12.4 and advertise
`ARCHS = 500,610,700,750,800,860,890,900` — no `120`. They run correctly on
RTX 50-series anyway, because the driver JIT-compiles the shipped `sm_90` PTX
onto Blackwell. Worth re-checking after any upstream bump: a build that ships
arch-specific cubins without PTX would break on these cards.

That JIT is not free, and it is why `Asr::start` pushes a throwaway utterance
through the model before accepting real ones. The very first inference on a
fresh machine cost **1299 ms**; once the driver has cached the compiled kernels
on disk, a cold process pays 221 ms and steady state is 90 ms. Warming up at
startup moves that cost off the user's first dictation.

This is also why the app does not use `faster-whisper`/CTranslate2, which
crashes with `CUBLAS_STATUS_NOT_SUPPORTED` on `sm_120` for int8 and has to be
forced to float16.

---

## Layout

```
src/                       HUD (React + TypeScript)
src-tauri/src/
  hotkey.rs                low-level keyboard hook, chord tracking
  audio.rs                 on-demand capture, ring buffer, resampling, WAV
  sound.rs                 session chimes
  sidecar.rs               job-object lifetime, readiness probing
  asr.rs                   whisper-server supervisor + client
  polish.rs                llama-server supervisor, cleanup prompt, guards
  inject.rs                clipboard save/restore, SendInput, focus target
  dictionary.rs            personal term list
  settings.rs              persisted settings, live-read by the pipeline
  setup.rs                 first-run downloader, install tracking
  pipeline.rs              session + utterance state machine
  lib.rs                   tray, window, wiring
src-tauri/nsis/hooks.nsh   installer hooks (user-guide Start-menu shortcut)
src-tauri/examples/
  cleanup.rs               cleanup quality harness (adversarial cases)
docs/user-guide.html       end-user documentation, bundled with the installer
scripts/setup.ps1          fetch runtimes + weights
scripts/test-asr.ps1       ASR contract + latency, pins the HTTP shape
vendor/                    whisper.cpp + llama.cpp binaries  (gitignored)
models/                    ggml + gguf weights               (gitignored)
```

## Tests

```powershell
cd src-tauri; cargo test --lib
```

53 unit tests, weighted toward the things that failed silently in development:
side-agnostic modifier mapping (a transposed `VK_LCONTROL` once made the hotkey
unpressable), ring-buffer marks surviving a wrap, resampling that keeps every
sample it owes including the tail, and the cleanup guards.

## Licence and credits

ParaFlow itself is MIT — see [LICENSE](LICENSE).

Nothing else is redistributed here. The inference runtimes and the speech models
are downloaded from their own publishers on first launch, which keeps this repo
small and means each component arrives under its own terms:

| component | what it does | licence |
|---|---|---|
| [parakeet.cpp](https://github.com/mudler/parakeet.cpp) | transcription runtime | MIT |
| [llama.cpp](https://github.com/ggml-org/llama.cpp) | cleanup runtime | MIT |
| [whisper.cpp](https://github.com/ggml-org/whisper.cpp) | optional fallback runtime | MIT |
| [Parakeet TDT 0.6B v3](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) | speech recognition model, by NVIDIA | CC-BY-4.0 |
| [Qwen3-4B-Instruct-2507](https://huggingface.co/Qwen/Qwen3-4B-Instruct-2507) | cleanup model, by Alibaba Qwen | Apache-2.0 |

The Parakeet model is CC-BY-4.0, which asks for attribution — hence this table,
and the credit in the app's own guide.

Not affiliated with [Wispr Flow](https://wisprflow.ai). ParaFlow was written
because that tool is good and sends your voice to a server; this one does the
same job without the network and without a subscription.
