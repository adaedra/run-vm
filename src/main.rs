use clap::Clap;
use futures::StreamExt;
use json::JsonValue;
use signal_hook_tokio::Signals;
use std::{io, path::PathBuf};

mod qemu;

#[derive(Clap, Debug)]
#[clap(version = env!("CARGO_PKG_VERSION"))]
struct Options {
    #[clap(
        default_value = ".",
        about = "The folder where to find the `options.txt` file"
    )]
    folder: PathBuf,
    #[clap(long, about = "Wait for a client to connect to VNC before starting")]
    wait_vnc: bool,
}

mod ioctl {
    use nix::ioctl_write_ptr_bad;

    const TIOCLINUX: u16 = 0x541C;

    ioctl_write_ptr_bad!(tioclinux, TIOCLINUX, u8);
}

const VT_CMD_BLANK: u8 = 14;
const VT_CMD_UNBLANK: u8 = 4;

fn blank_vt(blank: bool) -> io::Result<()> {
    let cmd = [if blank { VT_CMD_BLANK } else { VT_CMD_UNBLANK }];
    unsafe { ioctl::tioclinux(0, cmd.as_ptr()) }?;
    Ok(())
}

async fn handle_event(
    qemu: &mut qemu::Process,
    options: &Options,
    event: &JsonValue,
) -> anyhow::Result<()> {
    use log::debug;
    use nix::unistd::isatty;

    let name = event["event"].as_str().unwrap();
    debug!("Qemu: event {}", name);

    match name {
        "RESUME" if matches!(isatty(1), Ok(true)) => {
            blank_vt(true).ok();
        }
        "SHUTDOWN" if matches!(isatty(1), Ok(true)) => {
            blank_vt(false).ok();
        }
        "VNC_INITIALIZED" if options.wait_vnc => {
            let res = qemu
                .write(json::object! { "execute": "query-status" })
                .await;

            if let Ok(Some("prelaunch")) = res.as_ref().map(|data| data["status"].as_str()) {
                // Start the machine
                qemu.write(json::object! { "execute": "cont" }).await?;
            }
        }
        _ => (),
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use log::{debug, error, info, trace};
    use signal_hook::consts::signal;
    use std::{
        env,
        io::{BufRead, BufReader},
        process,
    };
    use tokio::select;

    {
        use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

        TermLogger::init(
            LevelFilter::Trace,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        )?;
    }

    let options = Options::parse();
    dbg!(&options);

    let root = options.folder.canonicalize()?;
    trace!("Working directory: {}", root.display());
    env::set_current_dir(&root)?;

    let qemu_flags = {
        use std::fs::File;

        let file = File::open(root.join("options.txt"))?;
        let reader = BufReader::new(file);

        let options = reader
            .lines()
            .map(Result::unwrap)
            .map(|s| s.trim().to_owned())
            .filter(|line| !(line.is_empty() || line.starts_with('#')));
        ["-nodefaults", "-qmp", "stdio", "-S"]
            .iter()
            .map(|str| str.to_string())
            .chain(options)
            .collect::<Vec<_>>()
    };

    trace!("QEMU Flags: {:?}", &qemu_flags);
    let mut child = qemu::Process::init(&qemu_flags).await?;
    info!("Qemu: Pre-launch OK");

    let cpus = child
        .write(json::object! { "execute": "query-cpus-fast" })
        .await?;
    for cpu in cpus.members() {
        use nix::{
            sched::{sched_setaffinity, CpuSet},
            unistd::Pid,
        };

        let index = cpu["cpu-index"].as_usize().unwrap();
        let pid = cpu["thread-id"].as_usize().unwrap();

        debug!("vCPU {} => PID {}", index, pid);
        let mut cpu_mask = CpuSet::new();
        cpu_mask.set(4 + index).ok();
        sched_setaffinity(Pid::from_raw(pid as libc::pid_t), &cpu_mask)?;
    }

    info!("Qemu: Ready");

    let signals = Signals::new(&[signal::SIGINT])?;
    let signal_handle = signals.handle();
    let mut signals = signals.fuse();

    if !options.wait_vnc {
        child.write(json::object! { "execute": "cont" }).await?;
    }

    loop {
        select! {
            biased;

            signal = signals.next(), if !signal_handle.is_closed() => {
                match signal {
                    Some(signal::SIGINT) => {
                        blank_vt(false).ok();
                        signal_handle.close();
                        child.write(json::object! { "execute": "quit" }).await.ok();
                    },
                    _ => (),
                }
            }
            event = child.read_event() => {
                let error = match event {
                    Ok(event) => handle_event(&mut child, &options, &event).await.err(),
                    Err(e) => Some(e),
                };

                if let Some(e) = error {
                    signal_handle.close();

                    if e.is::<qemu::Eof>() {
                        match child.finish().await {
                            Ok(res) if res.success() => return Ok(()),
                            Ok(res) => {
                                error!("Qemu: exit, {}", res);
                                process::exit(1);
                            }
                            Err(e) => return Err(e.into()),
                        }
                    } else {
                        error!("Error: {}", e);
                        return Err(e.into());
                    }
                }
            }
        };
    }
}
