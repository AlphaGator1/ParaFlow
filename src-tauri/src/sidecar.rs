//! Shared plumbing for the child processes that hold the models.
//!
//! Both inference backends run as local HTTP servers rather than in-process:
//! it keeps the GPU work off our threads, lets each model stay resident, and
//! means a crash in either one cannot take the app down with it.

use anyhow::{anyhow, Context, Result};
use std::process::Child;
use std::time::{Duration, Instant};

/// Bind a child's lifetime to ours using a Windows job object.
///
/// Without this, a hard kill of the app (Task Manager, a debugger detaching, a
/// panic) leaves the sidecar running and holding GPU memory, with no parent
/// left to shut it down. The job object makes that impossible: when our last
/// handle closes, Windows terminates everything in the job.
#[cfg(windows)]
pub fn tie_to_parent(child: &Child) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    unsafe {
        let job = CreateJobObjectW(None, None).context("CreateJobObject failed")?;

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .context("SetInformationJobObject failed")?;

        AssignProcessToJobObject(job, HANDLE(child.as_raw_handle() as _))
            .context("AssignProcessToJobObject failed")?;

        // `job` is deliberately never closed. HANDLE is a plain Copy wrapper
        // with no Drop, so letting it fall out of scope leaks the OS handle -
        // which is exactly what we want. The job stays alive for our whole
        // process lifetime, and when we exit the last handle closes and
        // KILL_ON_JOB_CLOSE takes the sidecar down with us.
    }
    Ok(())
}

/// Windows-only creation flag: run the sidecar without flashing up a console.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Refuse to start if something is already listening on the sidecar's port.
///
/// Windows will happily let a second process bind a port another one already
/// holds, and the loser simply never receives a connection. The failure that
/// produces is nasty: our readiness probe succeeds instantly against the
/// *other* process, and we then transcribe against whatever model it happens
/// to have loaded, with no error anywhere. This turns that into a startup
/// failure with a name attached.
pub fn ensure_port_free(name: &str, port: u16) -> Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
        Ok(_) => Err(anyhow!(
            "port {port} is already in use, so {name} cannot start. Another ParaFlow \
             instance is probably still running - close it and try again."
        )),
        // Refused is what we want: nothing is there.
        Err(_) => Ok(()),
    }
}

/// Block until the sidecar can actually serve on `port`, or it dies, or we
/// give up.
///
/// The two servers signal readiness differently, and the difference matters:
/// `whisper-server` does not bind its port until the model is loaded, so a
/// refused connection means "still loading". `llama-server` binds immediately
/// and answers **503 Service Unavailable** while the weights load. Treating a
/// connection as readiness would hand back a sidecar that rejects the next
/// several seconds of requests, so 503 explicitly does not count.
pub fn await_ready(
    name: &str,
    port: u16,
    path: &str,
    timeout: Duration,
    child: &mut Child,
) -> Result<Duration> {
    let began = Instant::now();
    let probe = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()?;
    let url = format!("http://127.0.0.1:{port}{path}");

    while began.elapsed() < timeout {
        if let Some(status) = child.try_wait()? {
            return Err(anyhow!("{name} exited during startup with {status}"));
        }
        match probe.get(&url).send() {
            Ok(r) if r.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE => {
                let took = began.elapsed();
                tracing::info!(name, ms = took.as_millis(), status = %r.status(), "sidecar ready");
                return Ok(took);
            }
            _ => std::thread::sleep(Duration::from_millis(150)),
        }
    }
    Err(anyhow!("{name} did not become ready within {timeout:?}"))
}
