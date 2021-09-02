use json::JsonValue;

mod qemu;

async fn handle_event(qemu: &mut qemu::Process, event: &JsonValue) -> anyhow::Result<()> {
    use log::debug;

    let name = event["event"].as_str().unwrap();
    debug!("Qemu: event {}", name);

    match name {
        "VNC_INITIALIZED" => {
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
    use std::{
        io::{BufRead, BufReader},
        process,
    };

    {
        use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

        TermLogger::init(
            LevelFilter::Trace,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        )?;
    }

    let root = {
        use std::{env, path::Path};

        let path = env::args().skip(1).next().unwrap_or_else(|| ".".to_owned());
        let root = Path::new(&path).canonicalize()?;

        trace!("Working directory: {}", root.display());
        env::set_current_dir(&root)?;
        root
    };

    let options = {
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

    trace!("QEMU Arguments: {:?}", &options);
    let mut child = qemu::Process::init(&options).await?;
    info!("Qemu: Ready");

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

    loop {
        let error = match child.read_event().await {
            Ok(event) => handle_event(&mut child, &event).await.err(),
            Err(e) => Some(e),
        };

        if let Some(e) = error {
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
}
