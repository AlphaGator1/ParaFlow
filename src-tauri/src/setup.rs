//! First-run setup: work out what this machine needs, then fetch it.
//!
//! Nothing large ships in the installer. Model weights are already quantised
//! and compress to ~95% of their original size, so bundling them would be paid
//! for at almost full price in the download either way - and doing it here
//! instead means the transfer can resume, can be verified, and can show real
//! progress, none of which an NSIS download plugin manages well.
//!
//! The user is asked exactly one question: transcription only, or transcription
//! plus cleanup. Everything else - which GPU backend, which files, where they
//! go - is decided here, because it is decidable and asking would only be
//! asking someone to look up their own graphics card.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Which GPU backend to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Accel {
    /// NVIDIA. Much faster for the cleanup model - 66 ms against Vulkan's
    /// 588 ms on a 4B model - at the cost of ~1.4 GB of cuBLAS.
    Cuda,
    /// Everyone else: AMD, Intel, and NVIDIA without the CUDA driver. Tiny,
    /// and fine for transcription; noticeably slower for cleanup.
    Vulkan,
}

impl Accel {
    /// Pick a backend by looking for the NVIDIA CUDA driver.
    ///
    /// `nvcuda.dll` is installed by the NVIDIA driver itself, not by the CUDA
    /// toolkit, so its presence means CUDA will actually run. Checking this
    /// rather than asking avoids making a non-technical user identify their
    /// own graphics card.
    pub fn detect() -> Self {
        #[cfg(windows)]
        {
            use windows::core::w;
            use windows::Win32::System::LibraryLoader::LoadLibraryW;
            // SAFETY: loading a well-known system DLL by name; the handle is
            // intentionally leaked, as the driver DLL stays mapped anyway.
            let has_cuda = unsafe { LoadLibraryW(w!("nvcuda.dll")).is_ok() };
            if has_cuda {
                return Accel::Cuda;
            }
        }
        Accel::Vulkan
    }

    pub fn slug(&self) -> &'static str {
        match self {
            Accel::Cuda => "cuda",
            Accel::Vulkan => "vulkan",
        }
    }
}

/// How much of ParaFlow to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Profile {
    /// Recogniser only. Parakeet already emits punctuation and casing, so this
    /// is a complete, usable dictation tool on its own.
    Transcribe,
    /// Recogniser plus the local instruction model that removes fillers,
    /// applies spoken self-corrections, and matches the target app's register.
    Polish,
}

/// Where a downloaded artifact ends up.
#[derive(Debug, Clone, Copy)]
enum Dest {
    /// A single file, kept under `models/` with its own name.
    Model,
    /// A zip whose contents are flattened into `runtimes/<dir>/`.
    ///
    /// Flattened because these archives nest their payload one directory deep,
    /// and the CUDA runtime DLLs arrive in a *separate* archive from the
    /// binaries that need them - they only work sitting side by side.
    RuntimeZip(&'static str),
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub label: &'static str,
    url: &'static str,
    dest: Dest,
    /// Download size, for the progress UI. Approximate; the real total comes
    /// from Content-Length at fetch time.
    pub approx_mb: u64,
}

const PARAKEET_TAG: &str = "v0.5.0";
const LLAMA_TAG: &str = "b10539";

/// Everything `profile` needs on `accel`, in the order it should be fetched.
pub fn plan(profile: Profile, accel: Accel) -> Vec<Asset> {
    let mut assets = Vec::new();

    match accel {
        Accel::Cuda => {
            assets.push(Asset {
                label: "Speech recogniser (NVIDIA)",
                url: "https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/parakeet-v0.5.0-bin-win-cuda-x64.zip",
                dest: Dest::RuntimeZip("parakeet-cuda"),
                approx_mb: 299,
            });
            assets.push(Asset {
                label: "NVIDIA maths libraries",
                url: "https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/cudart-parakeet-bin-win-cuda-x64.zip",
                dest: Dest::RuntimeZip("parakeet-cuda"),
                approx_mb: 553,
            });
        }
        Accel::Vulkan => {
            assets.push(Asset {
                label: "Speech recogniser",
                url: "https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/parakeet-v0.5.0-bin-win-vulkan-x64.zip",
                dest: Dest::RuntimeZip("parakeet-vulkan"),
                approx_mb: 34,
            });
        }
    }

    assets.push(Asset {
        label: "Speech model (Parakeet)",
        url: "https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/tdt-0.6b-v3-q8_0.gguf",
        dest: Dest::Model,
        approx_mb: 897,
    });

    if profile == Profile::Polish {
        match accel {
            Accel::Cuda => {
                assets.push(Asset {
                    label: "Cleanup engine (NVIDIA)",
                    url: "https://github.com/ggml-org/llama.cpp/releases/download/b10539/llama-b10539-bin-win-cuda-13.3-x64.zip",
                    dest: Dest::RuntimeZip("llama-cuda"),
                    approx_mb: 140,
                });
                assets.push(Asset {
                    label: "NVIDIA maths libraries (cleanup)",
                    url: "https://github.com/ggml-org/llama.cpp/releases/download/b10539/cudart-llama-bin-win-cuda-13.3-x64.zip",
                    dest: Dest::RuntimeZip("llama-cuda"),
                    approx_mb: 373,
                });
            }
            Accel::Vulkan => {
                assets.push(Asset {
                    label: "Cleanup engine",
                    url: "https://github.com/ggml-org/llama.cpp/releases/download/b10539/llama-b10539-bin-win-vulkan-x64.zip",
                    dest: Dest::RuntimeZip("llama-vulkan"),
                    approx_mb: 33,
                });
            }
        }
        assets.push(Asset {
            label: "Cleanup model (Qwen3 4B)",
            url: "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf",
            dest: Dest::Model,
            approx_mb: 2382,
        });
    }

    // Silence the unused-const warning while keeping the tags visible next to
    // the URLs they pin.
    let _ = (PARAKEET_TAG, LLAMA_TAG);
    assets
}

/// Total download for a profile, in MB, ignoring anything already on disk.
pub fn download_mb(profile: Profile, accel: Accel) -> u64 {
    plan(profile, accel).iter().map(|a| a.approx_mb).sum()
}

/// What still needs fetching, in MB.
///
/// `install` skips assets already present, so this - not the plan total - is
/// what the user will actually wait for. Showing the total instead overstates
/// the cost for anyone reinstalling, switching profile, or resuming after an
/// interruption, which is precisely when reassurance matters most.
pub fn remaining_mb(profile: Profile, accel: Accel) -> u64 {
    let Some(root) = install_root() else { return download_mb(profile, accel) };
    plan(profile, accel).iter().filter(|a| !landed(&root, a)).map(|a| a.approx_mb).sum()
}

/// `%LOCALAPPDATA%\ParaFlow`.
pub fn install_root() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ParaFlow"))
}

/// The two things dictation cannot start without, as repo-style relative paths.
///
/// Deliberately expressed the same way the pipeline asks for them, so that
/// "do we need setup?" is answered by the *same* lookup that will later go
/// find these files. A separate check that only looked at the install root
/// would show the setup screen to a developer who already has everything in
/// `vendor/`.
pub const ESSENTIAL_SERVERS: &[&str] =
    &["vendor/parakeet-cuda/parakeet-server.exe", "vendor/parakeet-vulkan/parakeet-server.exe"];
pub const ESSENTIAL_MODEL: &str = "models/tdt-0.6b-v3-q8_0.gguf";

/// Remove runtime directories that were never stamped complete.
///
/// A directory can exist without all its markers if an install was interrupted
/// mid-archive, or if it was written by a build that tracked completion
/// per-directory rather than per-archive. Either way the contents cannot be
/// trusted, and the failure is ugly and remote from the cause: the sidecar
/// dies at first use with a Windows dialog reading
/// "cublas64_12.dll was not found".
///
/// Deleting is safe - everything here is re-downloadable, and the models,
/// which are the expensive part, live elsewhere and are keyed by filename.
pub fn purge_incomplete() {
    let Some(root) = install_root() else { return };
    let runtimes = root.join("runtimes");
    if !runtimes.exists() {
        return;
    }

    // Every archive that targets each runtime directory, across all plans.
    let mut required: std::collections::HashMap<&'static str, Vec<Asset>> = Default::default();
    for profile in [Profile::Transcribe, Profile::Polish] {
        for accel in [Accel::Cuda, Accel::Vulkan] {
            for asset in plan(profile, accel) {
                if let Dest::RuntimeZip(dir) = asset.dest {
                    let entry = required.entry(dir).or_default();
                    if !entry.iter().any(|a| a.url == asset.url) {
                        entry.push(asset);
                    }
                }
            }
        }
    }

    for (dir, assets) in required {
        let path = runtimes.join(dir);
        if !path.exists() {
            continue;
        }
        let complete = assets.iter().all(|a| marker(&root, a).exists());
        if !complete {
            tracing::warn!(
                dir,
                "runtime directory is incomplete (a previous install was interrupted); \
                 removing it so setup can fetch it cleanly"
            );
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::error!(error = %e, dir, "could not remove incomplete runtime");
            }
        }
    }
}

/// Can we dictate at all? `resolve` is the app's own path lookup.
///
/// `PARAFLOW_FORCE_SETUP=1` forces the setup screen even on a machine that has
/// everything, which is the only way to look at it once you have installed
/// once. It does not delete anything: `install` skips assets already on disk,
/// so continuing from the forced screen is a no-op.
pub fn ready(resolve: impl Fn(&str) -> bool) -> bool {
    if std::env::var("PARAFLOW_FORCE_SETUP").is_ok() {
        tracing::info!("PARAFLOW_FORCE_SETUP set; showing setup screen");
        return false;
    }
    ESSENTIAL_SERVERS.iter().any(|p| resolve(p)) && resolve(ESSENTIAL_MODEL)
}

/// Stamp written once an archive has been fully extracted.
///
/// Runtimes need *two* archives unpacked into one directory - the binaries and
/// the CUDA maths libraries ship separately - so "is this directory populated?"
/// cannot answer "is this asset installed?". Asking whether the server binary
/// exists said yes as soon as the first archive landed, which silently skipped
/// the second and left a runtime with no cuBLAS beside it. A per-archive stamp
/// is the only thing that distinguishes them.
fn marker(root: &Path, asset: &Asset) -> PathBuf {
    root.join(".installed").join(file_name_of(asset.url))
}

fn landed(root: &Path, asset: &Asset) -> bool {
    match asset.dest {
        // Models are one file with a unique name, so the file is its own proof.
        Dest::Model => root.join("models").join(file_name_of(asset.url)).exists(),
        Dest::RuntimeZip(_) => marker(root, asset).exists(),
    }
}

fn file_name_of(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or("download.bin")
}

/// Progress for one asset, reported to the setup UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub label: String,
    pub index: usize,
    pub total: usize,
    pub bytes: u64,
    pub of_bytes: u64,
    pub done: bool,
    /// This asset was already on disk and nothing was transferred.
    ///
    /// Without this the UI cannot tell the two apart, and a skipped multi-GB
    /// model flashes past under its own name looking exactly like a download -
    /// which reads as "it re-downloaded everything" to anyone watching.
    pub skipped: bool,
}

/// Fetch and install everything in `plan`, reporting progress as it goes.
pub fn install(
    profile: Profile,
    accel: Accel,
    mut on_progress: impl FnMut(Progress),
) -> Result<PathBuf> {
    let root = install_root().ok_or_else(|| anyhow!("no LOCALAPPDATA to install into"))?;
    std::fs::create_dir_all(root.join("models"))?;
    std::fs::create_dir_all(root.join("runtimes"))?;

    let assets = plan(profile, accel);
    let total = assets.len();
    tracing::info!(
        ?profile,
        accel = accel.slug(),
        assets = total,
        mb = download_mb(profile, accel),
        root = %root.display(),
        "starting first-run install"
    );
    let client = reqwest::blocking::Client::builder()
        // Generous: these are multi-GB files over a link we do not control.
        .timeout(None)
        .build()?;

    for (index, asset) in assets.iter().enumerate() {
        if landed(&root, asset) {
            tracing::info!(label = asset.label, "already installed, skipping");
            on_progress(Progress {
                label: asset.label.into(),
                index,
                total,
                bytes: 0,
                of_bytes: 0,
                done: true,
                skipped: true,
            });
            continue;
        }

        let staged = fetch(&client, asset, &root, |bytes, of_bytes| {
            on_progress(Progress {
                label: asset.label.into(),
                index,
                total,
                bytes,
                of_bytes,
                done: false,
                skipped: false,
            })
        })?;

        match asset.dest {
            Dest::Model => {
                let final_path = root.join("models").join(file_name_of(asset.url));
                std::fs::rename(&staged, &final_path)
                    .with_context(|| format!("could not move {} into place", asset.label))?;
            }
            Dest::RuntimeZip(dir) => {
                unzip_flat(&staged, &root.join("runtimes").join(dir))
                    .with_context(|| format!("could not unpack {}", asset.label))?;
                let _ = std::fs::remove_file(&staged);
                // Only now, after a clean extraction, is this archive done.
                let stamp = marker(&root, asset);
                if let Some(parent) = stamp.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&stamp, asset.url)?;
            }
        }

        on_progress(Progress {
            label: asset.label.into(),
            index,
            total,
            bytes: 0,
            of_bytes: 0,
            done: true,
            skipped: false,
        });
    }

    Ok(root)
}

/// Download to a `.part` file beside the destination, resuming if one is there.
fn fetch(
    client: &reqwest::blocking::Client,
    asset: &Asset,
    root: &Path,
    mut on_bytes: impl FnMut(u64, u64),
) -> Result<PathBuf> {
    let part = root.join(format!("{}.part", file_name_of(asset.url)));
    let already = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

    let mut req = client.get(asset.url);
    if already > 0 {
        tracing::info!(label = asset.label, already, "resuming interrupted download");
        req = req.header(reqwest::header::RANGE, format!("bytes={already}-"));
    }

    let mut resp = req.send().with_context(|| format!("could not reach {}", asset.url))?;
    if !resp.status().is_success() {
        return Err(anyhow!("{} returned {}", asset.url, resp.status()));
    }

    // A server that ignores our Range header sends 200 and the whole file, in
    // which case appending would corrupt it - start over instead.
    let resuming = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut file = if resuming && already > 0 {
        let mut f = std::fs::OpenOptions::new().write(true).open(&part)?;
        f.seek(SeekFrom::End(0))?;
        f
    } else {
        std::fs::File::create(&part)?
    };
    let start = if resuming { already } else { 0 };
    let expect = resp.content_length().unwrap_or(0) + start;

    let mut written = start;
    let mut buf = vec![0u8; 1 << 20];
    let mut since_report = 0u64;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        written += n as u64;
        since_report += n as u64;
        // Roughly every 4 MB; enough for a smooth bar without flooding the UI.
        if since_report >= 4 << 20 {
            on_bytes(written, expect);
            since_report = 0;
        }
    }
    file.flush()?;
    drop(file);

    if expect > 0 && written != expect {
        return Err(anyhow!(
            "{} ended early: got {written} bytes, expected {expect}. It will resume if you retry.",
            asset.label
        ));
    }
    on_bytes(written, expect);
    Ok(part)
}

/// Extract every file in `zip` into `dir`, discarding the archive's internal
/// directory structure.
fn unzip_flat(zip_path: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        // Take only the base name. This both flattens the archive and makes
        // path traversal via "../" entries impossible.
        let name = match entry.enclosed_name().and_then(|p| p.file_name().map(|f| f.to_owned())) {
            Some(n) => n,
            None => continue,
        };
        let mut out = std::fs::File::create(dir.join(name))?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcribe_only_skips_the_cleanup_assets() {
        let p = plan(Profile::Transcribe, Accel::Cuda);
        assert!(!p.iter().any(|a| a.label.contains("Cleanup")));
        assert!(p.iter().any(|a| a.label.contains("Speech model")));
    }

    #[test]
    fn two_archives_sharing_one_runtime_dir_are_tracked_separately() {
        // The regression this guards: the CUDA plan unpacks the binaries and
        // the cuBLAS libraries into the same directory. An "is the directory
        // populated?" check marked the second one done as soon as the first
        // landed, leaving a runtime that could not start.
        let dir = std::env::temp_dir().join("paraflow-marker-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cuda = plan(Profile::Transcribe, Accel::Cuda);
        let zips: Vec<_> =
            cuda.iter().filter(|a| matches!(a.dest, Dest::RuntimeZip(_))).collect();
        assert_eq!(zips.len(), 2, "CUDA parakeet should need two archives");
        assert_ne!(
            marker(&dir, zips[0]),
            marker(&dir, zips[1]),
            "archives sharing a directory must have distinct markers"
        );

        // Marking the first must not make the second look finished.
        std::fs::create_dir_all(marker(&dir, zips[0]).parent().unwrap()).unwrap();
        std::fs::write(marker(&dir, zips[0]), "x").unwrap();
        assert!(landed(&dir, zips[0]));
        assert!(!landed(&dir, zips[1]), "second archive must still be pending");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn polish_adds_a_cleanup_engine_and_model() {
        let p = plan(Profile::Polish, Accel::Cuda);
        assert!(p.iter().any(|a| a.label.contains("Cleanup engine")));
        assert!(p.iter().any(|a| a.label.contains("Cleanup model")));
    }

    #[test]
    fn vulkan_saves_the_cublas_bulk() {
        // The whole reason Vulkan is offered. Note the saving is entirely in
        // the runtime - both profiles download the same 897 MB speech model,
        // so the *ratio* is nowhere near the ~25x the runtimes differ by.
        let cuda = download_mb(Profile::Transcribe, Accel::Cuda);
        let vulkan = download_mb(Profile::Transcribe, Accel::Vulkan);
        let saved = cuda - vulkan;
        assert!(saved > 700, "expected a large runtime saving, got {saved} MB");
    }

    #[test]
    fn cleanup_is_the_bulk_of_the_download() {
        let bare = download_mb(Profile::Transcribe, Accel::Cuda);
        let full = download_mb(Profile::Polish, Accel::Cuda);
        assert!(full > bare * 2, "cleanup should dominate: {bare} MB -> {full} MB");
    }

    /// Exercises the real download and extraction path end to end against the
    /// smallest asset we ship (~34 MB), including the resume branch.
    ///
    /// Ignored by default because it hits the network; run explicitly with
    /// `cargo test --lib download_and_unzip -- --ignored --nocapture`.
    #[test]
    #[ignore = "network"]
    fn download_and_unzip_a_real_asset() {
        let dir = std::env::temp_dir().join("paraflow-setup-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let asset = plan(Profile::Transcribe, Accel::Vulkan)
            .into_iter()
            .find(|a| matches!(a.dest, Dest::RuntimeZip(_)))
            .expect("vulkan plan has a runtime zip");

        let client = reqwest::blocking::Client::builder().timeout(None).build().unwrap();

        // First pass: fetch a prefix, then truncate it to simulate a drop.
        let mut seen_progress = false;
        let staged =
            fetch(&client, &asset, &dir, |_, _| seen_progress = true).expect("download failed");
        assert!(seen_progress, "progress callback never fired");
        let full_len = std::fs::metadata(&staged).unwrap().len();
        assert!(full_len > 1_000_000, "suspiciously small download: {full_len}");

        // Truncate and re-fetch: the resume branch should top it back up to
        // exactly the same size rather than appending a second copy.
        {
            let f = std::fs::OpenOptions::new().write(true).open(&staged).unwrap();
            f.set_len(full_len / 2).unwrap();
        }
        let resumed = fetch(&client, &asset, &dir, |_, _| {}).expect("resume failed");
        assert_eq!(
            std::fs::metadata(&resumed).unwrap().len(),
            full_len,
            "resume produced the wrong size"
        );

        let out = dir.join("runtimes-test");
        unzip_flat(&resumed, &out).expect("unzip failed");
        let files: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).collect();
        assert!(!files.is_empty(), "nothing extracted");
        assert!(
            out.join("parakeet-server.exe").exists(),
            "flattening did not put the server binary at the top level"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn urls_yield_sensible_file_names() {
        assert_eq!(file_name_of("https://x/y/model.gguf"), "model.gguf");
        for a in plan(Profile::Polish, Accel::Cuda) {
            assert!(!file_name_of(a.url).is_empty());
            assert!(a.url.starts_with("https://"), "assets must be fetched over TLS");
        }
    }
}
