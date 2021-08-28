use std::{
    env::args,
    fs::File,
    io::{self, BufRead, BufReader},
    path::Path,
    process::Command,
};

fn main() -> io::Result<()> {
    let root = {
        let path = args().skip(1).next().unwrap_or_else(|| ".".to_owned());
        Path::new(&path).canonicalize()?
    };

    let options = {
        let file = File::open(root.join("options.txt"))?;
        let reader = BufReader::new(file);

        let options = reader
            .lines()
            .map(Result::unwrap)
            .map(|s| s.trim().to_owned())
            .filter(|line| !(line.is_empty() || line.starts_with('#')));
        ["-nodefaults", "-monitor", "stdio", "-S"]
            .iter()
            .map(|str| str.to_string())
            .chain(options)
            .collect::<Vec<_>>()
    };

    let status = Command::new("qemu-system-x86_64").args(options).status()?;
    println!("{:?}", status);

    Ok(())
}
