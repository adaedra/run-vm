use futures::channel::{mpsc, oneshot};
use json::JsonValue;
use std::{error, fmt, process::ExitStatus};
use tokio::{
    io::BufReader,
    process::{Child, ChildStdin, ChildStdout},
    select,
    task::JoinHandle,
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
    worker: JoinHandle<ExitStatus>,
    event_queue: mpsc::Receiver<JsonValue>,
    reply_queue: mpsc::Sender<(JsonValue, oneshot::Sender<JsonValue>)>,
}

impl Process {
    pub async fn init(args: &[String]) -> anyhow::Result<Process> {
        use log::{debug, trace};
        use std::process::Stdio;
        use tokio::{io::AsyncBufReadExt, process::Command, task};

        let mut child = Command::new("qemu-system-x86_64")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        let mut greeting = String::new();
        if stdout.read_line(&mut greeting).await? == 0 {
            trace!("Qemu(pre): Wait");
            child.wait().await.unwrap();
            return Err(Eof.into());
        }
        trace!("QMP: Recv: {}", greeting.trim());

        let json = json::parse(&greeting)?;
        let version = Version::from_json(&json["QMP"]["version"]["qemu"]);
        debug!("QMP: Connected, version {}", version);

        let (event_tx, event_rx) = mpsc::channel(1);
        let (reply_tx, reply_rx) = mpsc::channel(1);

        let worker = task::spawn(qemu_worker(event_tx, reply_rx, child, stdin, stdout));

        let mut p = Process {
            worker,
            event_queue: event_rx,
            reply_queue: reply_tx,
        };

        match p
            .write(json::object! { "execute": "qmp_capabilities" })
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

    pub async fn write(&mut self, data: JsonValue) -> anyhow::Result<JsonValue> {
        use futures::SinkExt;

        let (tx, rx) = oneshot::channel();
        self.reply_queue.send((data, tx)).await?;

        match rx.await {
            Ok(reply) => Ok(reply),
            Err(_) => Err(Eof.into()),
        }
    }

    pub async fn read_event(&mut self) -> anyhow::Result<JsonValue> {
        use futures::StreamExt;

        match self.event_queue.next().await {
            Some(event) => Ok(event),
            None => Err(Eof.into()),
        }
    }

    pub async fn finish(self) {
        use log::{error, trace};
        trace!("Qemu: Wait");

        let res = self.worker.await.unwrap();
        if !res.success() {
            error!("Qemu: exited, {}", res);
        }
    }
}

async fn qemu_worker(
    mut event_tx: mpsc::Sender<JsonValue>,
    mut reply_rx: mpsc::Receiver<(JsonValue, oneshot::Sender<JsonValue>)>,
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
) -> ExitStatus {
    use futures::{SinkExt, StreamExt};
    use log::{error, trace};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mut reply_waiter = None;
    let mut lines = stdout.lines();

    loop {
        select! {
            biased;

            msg = reply_rx.next(), if reply_waiter.is_none() => {
                let (data, reply_tx) = msg.unwrap();
                reply_waiter = Some(reply_tx);

                let reply_buf = data.to_string();
                trace!("QMP: Send: {}", &reply_buf);
                stdin.write_all(reply_buf.as_bytes()).await.unwrap();
            }
            read = lines.next_line() => {
                let json = match read {
                    Ok(None) => break,
                    Ok(Some(line)) => line,
                    Err(e) => panic!("{}", e),
                };

                trace!("QMP: Recv: {}", json.trim());
                let data = json::parse(&json).unwrap();

                if data.has_key("return") {
                    if let Some(waiter) = reply_waiter.take() {
                        waiter.send(data["return"].clone()).unwrap();
                    } else {
                        error!("Message reply without waiter");
                    }
                } else {
                    event_tx.send(data).await.unwrap();
                }
            }
        };
    }

    child.wait().await.unwrap()
}
