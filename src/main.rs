mod qemu {
    use json::JsonValue;
    use std::fmt;

    pub struct Version(u8, u8, u8);

    impl Version {
        pub fn from_json(json: &JsonValue) -> Version {
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
}

fn main() -> anyhow::Result<()> {
    use log::debug;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

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
    let mut qemu = Command::new("qemu-system-x86_64")
        .args(options)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let mut qemu_in = qemu.stdin.take().unwrap();
    let qemu_out = qemu.stdout.take().unwrap();
    let mut reader = BufReader::new(qemu_out);

    let mut version = Option::<qemu::Version>::None;

    loop {
        // for line in reader.lines() {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => (),
            Err(e) => return Err(e.into()),
        };

        debug!("QMP Read: {}", line);

        if version.is_none() {
            use std::io::Write;

            version = Some({
                let json = json::parse(&line)?;
                let version = qemu::Version::from_json(&json["QMP"]["version"]["qemu"]);
                debug!("QMP Connected, version {}", version);

                version
            });

            let msg = json::object! { "execute": "qmp_capabilities" };
            writeln!(qemu_in, "{}", msg.to_string())?;

            let mut reply = String::new();
            match reader.read_line(&mut reply) {
                Ok(0) => break,
                Ok(_) => (),
                Err(e) => return Err(e.into()),
            };
            debug!("QMP Read: {}", reply);

            let reply_json = json::parse(&reply)?;
            if !reply_json.has_key("return") {
                panic!("Error initializing QMP channel");
            }
        }
    }

    let status = qemu.wait()?;
    println!("{:?}", status);

    Ok(())
}
