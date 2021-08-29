mod qemu {
    use json::JsonValue;
    use log::debug;
    use std::{collections::VecDeque, error, fmt};
    use tokio::{
        io::BufReader,
        process::{Child, ChildStdin, ChildStdout},
    };

    pub struct Version(u8, u8, u8);

    impl Version {
        fn from_json(json: &JsonValue) -> Version {
            Version(
                json["major"].as_u8().unwrap(),
                json["minor"].as_u8().unwrap(),
                json["micro"].as_u8().unwrap(),
            )
        }
    }

    impl fmt::Display for Version {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "{}.{}.{}", self.0, self.1, self.2)
        }
    }

    pub struct Eof;

    impl fmt::Debug for Eof {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "QMP Channel Closed")
        }
    }

    impl fmt::Display for Eof {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            (self as &dyn fmt::Debug).fmt(f)
        }
    }

    impl error::Error for Eof {}

    pub struct Process {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,

        event_cache: VecDeque<JsonValue>,
    }

    impl Process {
        pub async fn init(args: &[String]) -> anyhow::Result<Process> {
            use std::process::Stdio;
            use tokio::{io::AsyncBufReadExt, process::Command};

            let mut child = Command::new("qemu-system-x86_64")
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;

            let stdin = child.stdin.take().unwrap();
            let stdout = BufReader::new(child.stdout.take().unwrap());

            let mut p = Process {
                child,
                stdin,
                stdout,
                event_cache: VecDeque::new(),
            };

            let mut greeting = String::new();
            if p.stdout.read_line(&mut greeting).await? == 0 {
                p.finish().await;
                return Err(Eof.into());
            }
            debug!("QMP: Recv {}", greeting.trim());

            let json = json::parse(&greeting)?;
            let version = Version::from_json(&json["QMP"]["version"]["qemu"]);
            debug!("QMP: Connected, version {}", version);

            match p
                .write(&json::object! { "execute": "qmp_capabilities" })
                .await
            {
                Ok(_) => (),
                Err(e) if e.is::<Eof>() => {
                    p.finish().await;
                    return Err(e.into());
                }
                Err(e) => return Err(e.into()),
            }

            Ok(p)
        }

        pub async fn write(&mut self, data: &JsonValue) -> anyhow::Result<JsonValue> {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

            debug!("QMP: Send {}", data);
            let mut msg = data.to_string();
            msg.push('\n');
            self.stdin.write_all(msg.as_bytes()).await?;

            loop {
                let mut line = String::new();
                if self.stdout.read_line(&mut line).await? == 0 {
                    return Err(Eof.into());
                }
                debug!("QMP: Recv {}", line.trim());

                let json = json::parse(&line)?;
                if json.has_key("event") {
                    self.event_cache.push_back(json);
                } else {
                    return Ok(json["return"].clone());
                }
            }
        }

        pub async fn read_event(&mut self) -> anyhow::Result<JsonValue> {
            use tokio::io::AsyncBufReadExt;

            if let Some(v) = self.event_cache.pop_front() {
                return Ok(v);
            }

            let mut event = String::new();
            if self.stdout.read_line(&mut event).await? == 0 {
                return Err(Eof.into());
            }

            debug!("QMP: Recv {}", event);
            Ok(json::parse(&event)?)
        }

        pub async fn finish(mut self) {
            use log::error;
            debug!("Qemu: wait");

            match self.child.wait().await {
                Ok(s) if s.success() => (),
                Ok(s) => error!("Qemu: exit, {}", s),
                Err(e) => error!("Qemu: error, {}", e),
            };
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use log::{debug, info};
    use std::io::{BufRead, BufReader};

    {
        use simplelog::{ColorChoice, Config, LevelFilter, TermLogger, TerminalMode};

        TermLogger::init(
            LevelFilter::Debug,
            Config::default(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        )?;
    }

    let root = {
        use std::{env, path::Path};

        let path = env::args().skip(1).next().unwrap_or_else(|| ".".to_owned());
        let root = Path::new(&path).canonicalize()?;

        debug!("Working directory: {}", root.display());
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

    debug!("QEMU Arguments: {:?}", &options);
    let mut child = qemu::Process::init(&options).await?;
    info!("Qemu: Ready");

    loop {
        let event = child.read_event().await;
        println!("{:?}", event);

        match event {
            Ok(_) => (),
            Err(e) => {
                child.finish().await;
                return Err(e);
            }
        }
    }
}
