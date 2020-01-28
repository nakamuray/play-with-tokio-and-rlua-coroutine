use rlua::prelude::*;
use rlua::{Function, Nil, RegistryKey, Thread, ThreadStatus, UserData, UserDataMethods, Value};
use std::env;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task;
use tokio::time::delay_for;

#[derive(Clone)]
enum IO {
    Nop,
    Sleep(u64),
    Fork(Arc<RegistryKey>),
    Get(String),
    Job {
        receiver: Arc<Mutex<mpsc::Receiver<Arc<RegistryKey>>>>,
    },
}
impl UserData for IO {}

struct Job(Arc<Mutex<mpsc::Receiver<Arc<RegistryKey>>>>);
impl UserData for Job {
    fn add_methods<'lua, M: UserDataMethods<'lua, Self>>(methods: &mut M) {
        methods.add_method("wait", |_, job, ()| {
            Ok(IO::Job {
                receiver: job.0.clone(),
            })
        });
    }
}

enum CoroutineStatus {
    Running { yielded: IO },
    Finished { retvalue: RegistryKey },
}

enum ResumeData {
    Nil,
    String(String),
    Key(Arc<RegistryKey>),
    Job(Job),
}

#[derive(Clone)]
struct App(Arc<Mutex<Lua>>);

impl App {
    const MAIN: &'static str = "MAIN";

    fn new() -> Self {
        let lua = Lua::new();
        Self::init(&lua);
        Self(Arc::new(Mutex::new(lua)))
    }
    fn init(lua: &Lua) {
        lua.context(|lua| {
            lua.globals()
                .set(
                    "nop",
                    lua.create_function(|_, _: Value| Ok(IO::Nop)).unwrap(),
                )
                .unwrap();

            lua.globals()
                .set(
                    "sleep",
                    lua.create_function(|_, sec| Ok(IO::Sleep(sec))).unwrap(),
                )
                .unwrap();

            lua.globals()
                .set(
                    "forkio",
                    lua.create_function(|lua, func: Function| {
                        let key = lua.create_registry_value(func)?;
                        Ok(IO::Fork(Arc::new(key)))
                    })
                    .unwrap(),
                )
                .unwrap();

            lua.globals()
                .set(
                    "get",
                    lua.create_function(|_, url| Ok(IO::Get(url))).unwrap(),
                )
                .unwrap();
        });
    }
    async fn load(&self, path: &Path) {
        let mut f = File::open(path).unwrap();
        let mut script = String::new();
        f.read_to_string(&mut script).unwrap();

        self.0.lock().await.context(|lua| {
            let main: Function = lua
                .load(&script)
                .set_name(path.to_str().unwrap())
                .unwrap()
                .eval()
                .unwrap();
            lua.set_named_registry_value(Self::MAIN, main).unwrap();
        });
    }
    async fn main(self) {
        let key = self.0.lock().await.context(|lua| {
            let main: Function = lua.named_registry_value(Self::MAIN).unwrap();
            let coro: Thread = lua.create_thread(main).unwrap();
            lua.create_registry_value(coro).unwrap()
        });
        self.run_coroutine(Arc::new(key), None).await
    }
    fn run_coroutine(
        self,
        key: Arc<RegistryKey>,
        out: Option<mpsc::Sender<Arc<RegistryKey>>>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        // XXX: async fn で再起的なことしようとすると、なんかエラーになる。
        //      というのを回避するために色々やってたらこんなんになった。何か無駄なことをしているかもしれない。
        Box::pin(async move {
            let mut data = ResumeData::Nil;
            loop {
                let status = self.resume_coroutine(&key, data).await;
                match status {
                    CoroutineStatus::Finished { retvalue: key } => {
                        if let Some(mut out) = out {
                            out.send(Arc::new(key)).await.unwrap();
                        }
                        break;
                    }
                    CoroutineStatus::Running { yielded: io } => {
                        data = ResumeData::Nil;
                        match io {
                            IO::Nop => task::yield_now().await,
                            IO::Sleep(sec) => delay_for(Duration::from_secs(sec)).await,
                            IO::Fork(k) => {
                                let key = self.0.lock().await.context(|lua| {
                                    let func: Function = lua.registry_value(&k).unwrap();
                                    let coro: Thread = lua.create_thread(func).unwrap();
                                    lua.create_registry_value(coro).unwrap()
                                });
                                let this = self.clone();
                                let (tx, rx) = mpsc::channel(1);
                                task::spawn(this.run_coroutine(Arc::new(key), Some(tx)));
                                data = ResumeData::Job(Job(Arc::new(Mutex::new(rx))));
                            }
                            IO::Get(url) => {
                                let r = reqwest::get(&url).await.unwrap();
                                data = ResumeData::String(r.text().await.unwrap());
                            }
                            IO::Job { receiver: rx } => {
                                let r = rx.lock().await.recv().await;
                                if let Some(key) = r {
                                    data = ResumeData::Key(key);
                                } else {
                                    // すでに wait され済みの job の場合
                                    data = ResumeData::Nil;
                                }
                            }
                        }
                    }
                }
            }
        })
    }
    async fn resume_coroutine(&self, key: &RegistryKey, data: ResumeData) -> CoroutineStatus {
        self.0.lock().await.context(|lua| {
            lua.expire_registry_values();

            let data = match data {
                ResumeData::Nil => Nil,
                ResumeData::String(s) => s.to_lua(lua).unwrap(),
                ResumeData::Key(key) => lua.registry_value(&key).unwrap(),
                ResumeData::Job(job) => job.to_lua(lua).unwrap(),
            };
            let coro: Thread = lua.registry_value(&key).unwrap();

            assert!(coro.status() == ThreadStatus::Resumable);

            let ret: Value = coro.resume(data).unwrap();
            match coro.status() {
                ThreadStatus::Resumable => match &ret {
                    Value::UserData(u) => {
                        if let Ok(io) = u.borrow::<IO>() {
                            CoroutineStatus::Running {
                                yielded: io.clone(),
                            }
                        } else {
                            panic!("unexpected value yielded: {:?}", ret)
                        }
                    }
                    _ => panic!("unexpected value yielded: {:?}", ret),
                },
                ThreadStatus::Unresumable => {
                    let key = lua.create_registry_value(ret).unwrap();
                    CoroutineStatus::Finished { retvalue: key }
                }
                ThreadStatus::Error => todo!("coroutine error case"),
            }
        })
    }
}

#[tokio::main]
async fn main() {
    let mut args = env::args();
    args.next().unwrap();
    let path = args.next().expect("script filename required");
    let path = Path::new(&path);
    let app = App::new();
    app.load(&path).await;
    app.main().await;
}
