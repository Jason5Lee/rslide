use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use actix::{
    fut, Actor, ActorContext, ActorFuture, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web::dev::Server;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder};
use actix_web_actors::ws;

#[derive(actix::Message)]
#[rtype(result = "()")]
enum MoveAction {
    Next,
    Prev,
}

use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    pub listen: String,
    pub template_page: String,
    pub page_list: String,
    pub assets_dir: Option<String>,
    pub heartbeat: Option<String>,
    pub timeout: Option<String>,
    pub prev_code: Option<u32>,
    pub next_code: Option<u32>,
}

mod hook {
    use super::MoveAction;
    use actix::Recipient;
    use crossbeam::utils::Backoff;
    use std::os::raw;
    use std::sync::atomic::{self, AtomicU8};
    use winapi::shared::minwindef::{DWORD, LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::{HHOOK, HWND, POINT};
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::winuser::{
        DispatchMessageW, GetMessageW, TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT,
        WM_KEYDOWN,
    };
    use winapi::um::{
        libloaderapi::GetModuleHandleW,
        winuser::{CallNextHookEx, SetWindowsHookExW, WH_KEYBOARD_LL},
    };
    const NOT_START: u8 = 0;
    const STARTING: u8 = 1;
    const STARTED: u8 = 2;

    static STATUS: AtomicU8 = AtomicU8::new(NOT_START);

    static mut HOOK: HHOOK = std::ptr::null_mut();
    static mut ACTION_RECIPIENT: Option<Recipient<MoveAction>> = None;
    static mut PREV_CODE: u32 = 26;
    static mut NEXT_CODE: u32 = 27;

    pub(crate) struct Hook {}

    impl Hook {
        pub(crate) fn new(
            prev_code: Option<u32>,
            next_code: Option<u32>,
            action_recipient: Recipient<MoveAction>,
        ) -> Self {
            if STATUS
                .compare_exchange(
                    NOT_START,
                    STARTING,
                    atomic::Ordering::Acquire,
                    atomic::Ordering::Acquire,
                )
                .is_err()
            {
                panic!("Hook already started")
            }
            // SAFETY: only executes when STATUS changes from NOT_START to STARTING, which will happen only once.
            unsafe {
                if let Some(prev_code) = prev_code {
                    PREV_CODE = prev_code
                }
                if let Some(next_code) = next_code {
                    NEXT_CODE = next_code
                }

                ACTION_RECIPIENT = Some(action_recipient);
            }

            // We spawn new thread because otherwise Hook doesn't work.
            // I don't have too much experience of Windows Hook, issues of better implementation are welcomed.
            std::thread::spawn(move || {
                unsafe {
                    // SAFETY: the calling function return only after STATUS becomes STARTED.
                    HOOK = SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(lpfn),
                        GetModuleHandleW(std::ptr::null_mut()),
                        0,
                    );

                    let err = GetLastError();
                    if err != 0 {
                        panic!("setup hook error with code {}", err)
                    }
                }

                STATUS.store(STARTED, atomic::Ordering::Release);

                let mut msg: winapi::um::winuser::MSG = winapi::um::winuser::MSG {
                    hwnd: 0 as HWND,
                    message: 0 as UINT,
                    wParam: 0 as WPARAM,
                    lParam: 0 as LPARAM,
                    time: 0 as DWORD,
                    pt: POINT { x: 0, y: 0 },
                };
                loop {
                    unsafe {
                        let pm = GetMessageW(&mut msg, 0 as HWND, 0, 0);
                        if pm == 0 {
                            break;
                        }

                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            });
            let backoff = Backoff::new();
            loop {
                if STATUS.load(atomic::Ordering::Acquire) == STARTED {
                    return Self {};
                }
                backoff.spin();
            }
        }
    }

    impl Drop for Hook {
        fn drop(&mut self) {
            // SAFETY: `Hook` is designed to construct not more than once,
            // this unhook won't be called more than once.
            // The new method ensure that HOOK won't be writeen after return.
            unsafe {
                UnhookWindowsHookEx(HOOK);
            }
        }
    }

    // SAFETY: when hook is setting, the `PREV_CODE`, `NEXT_CODE` and `ACTION_RECIPIENT` won't change.
    unsafe extern "system" fn lpfn(code: raw::c_int, w_param: WPARAM, l_param: LPARAM) -> LRESULT {
        let key = l_param as *const KBDLLHOOKSTRUCT;
        if w_param == WM_KEYDOWN as usize {
            let scan_code = (*key).scanCode;
            if scan_code == PREV_CODE {
                ACTION_RECIPIENT
                    .as_ref()
                    .unwrap()
                    .do_send(MoveAction::Prev)
                    .ok();
            } else if scan_code == NEXT_CODE {
                ACTION_RECIPIENT
                    .as_ref()
                    .unwrap()
                    .do_send(MoveAction::Next)
                    .ok();
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, w_param, l_param)
    }
}

#[actix_web::main]
async fn main() {
    dotenv::dotenv().ok();
    let config = envy::from_env::<Config>().expect("unable to load config");
    let server = setup_http_server(config);
    server.await.expect("failed to run server")
}
const JS_CODE: &str = r#"
<script>
var socket = new WebSocket("ws://" + window.location.hostname + ":" + window.location.port + "/content/");
var elem = document.getElementById("mainHtml");
socket.onmessage = function(event) {
    elem.innerHTML = event.data;
}
</script>"#;

async fn index(index_html: String) -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=UTF-8")
        .body(index_html)
}

async fn content_ws(
    page_actor: Addr<PageActor>,
    heartbeat: Duration,
    timeout: Duration,
    req: HttpRequest,
    stream: web::Payload,
) -> actix_web::Result<HttpResponse> {
    ws::start(
        PageWsActor::new(page_actor, heartbeat, timeout),
        &req,
        stream,
    )
}

async fn terminate(page_actor: Addr<PageActor>) -> impl Responder {
    page_actor.do_send(Terminate);

    HttpResponse::NoContent()
}
#[derive(actix::Message)]
#[rtype(result = "()")]
struct Terminate;

#[derive(actix::Message)]
#[rtype(result = "u32")]
struct Subscribe(Addr<PageWsActor>);
#[derive(actix::Message)]
#[rtype(result = "()")]
struct Unsubscribe(u32);

struct PageActor {
    hook: Option<hook::Hook>,
    pages: Vec<PathBuf>,
    subscribers: HashMap<u32, Addr<PageWsActor>>,
    next_sub_id: u32,
    cnt: usize,
    prev_code: Option<u32>,
    next_code: Option<u32>,
}
impl Actor for PageActor {
    type Context = actix::Context<PageActor>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hook = Some(hook::Hook::new(
            self.prev_code,
            self.next_code,
            ctx.address().recipient(),
        ))
    }

    fn stopped(&mut self, _: &mut Self::Context) {
        self.hook = None;
    }
}
impl Handler<MoveAction> for PageActor {
    type Result = ();

    fn handle(&mut self, msg: MoveAction, _: &mut Self::Context) -> Self::Result {
        match msg {
            MoveAction::Next => {
                if self.cnt + 1 < self.pages.len() {
                    self.cnt += 1;
                    let page = self.get_current_page();
                    for subscriber in self.subscribers.values() {
                        subscriber.do_send(NewPage(page.clone()))
                    }
                }
            }
            MoveAction::Prev => {
                if self.cnt > 0 {
                    self.cnt -= 1;
                    let page = self.get_current_page();
                    for subscriber in self.subscribers.values() {
                        subscriber.do_send(NewPage(page.clone()))
                    }
                }
            }
        }
    }
}
impl Handler<Terminate> for PageActor {
    type Result = ();

    fn handle(&mut self, _: Terminate, _: &mut Self::Context) -> Self::Result {
        self.hook = None;
    }
}
impl Handler<Subscribe> for PageActor {
    type Result = u32;

    fn handle(&mut self, msg: Subscribe, _: &mut Self::Context) -> Self::Result {
        let id = self.next_sub_id;
        self.next_sub_id += 1;

        msg.0.do_send(NewPage(self.get_current_page()));
        self.subscribers.insert(id, msg.0);

        id
    }
}
impl Handler<Unsubscribe> for PageActor {
    type Result = ();

    fn handle(&mut self, msg: Unsubscribe, _: &mut Self::Context) -> Self::Result {
        self.subscribers.remove(&msg.0);
    }
}

impl PageActor {
    fn load(page_list: &str, prev_code: Option<u32>, next_code: Option<u32>) -> Self {
        let file = File::open(page_list).expect("unable to open page list");
        let mut reader = BufReader::new(file);

        let mut list_path = PathBuf::from(page_list);
        list_path.pop();

        let mut pages = Vec::new();
        let mut line: String = String::new();
        loop {
            if reader
                .read_line(&mut line)
                .expect("failed to read pages list")
                == 0
            {
                break;
            }
            let relate = line.trim();
            if relate.is_empty() {
                line = String::new();
                continue;
            }
            let mut page_path = list_path.clone();
            page_path.push(relate);
            line = String::new();
            pages.push(page_path);
        }
        Self {
            hook: None,
            cnt: 0,
            subscribers: HashMap::new(),
            pages,
            next_sub_id: 0,
            prev_code,
            next_code,
        }
    }

    fn get_current_page(&self) -> String {
        fs::read_to_string(&self.pages[self.cnt])
            .unwrap_or_else(|err| format!("Internal Server Error: {}", err))
    }
}

#[derive(actix::Message)]
#[rtype(result = "()")]
struct NewPage(pub String);

struct PageWsActor {
    page_actor: Addr<PageActor>,
    heartbeat: Duration,
    timeout: Duration,
    last_hb: Instant,
    id: u32,
}

impl PageWsActor {
    fn new(page_actor: Addr<PageActor>, heartbeat: Duration, timeout: Duration) -> Self {
        Self {
            page_actor,
            heartbeat,
            timeout,
            last_hb: Instant::now(),
            id: 0,
        }
    }
}

impl actix::Actor for PageWsActor {
    type Context = ws::WebsocketContext<PageWsActor>;

    fn started(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);

        let addr = ctx.address();
        self.page_actor
            .send(Subscribe(addr))
            .into_actor(self)
            .then(|res, act, ctx| {
                match res {
                    Ok(res) => act.id = res,
                    Err(_) => ctx.stop(),
                }
                fut::ready(())
            })
            .wait(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        self.page_actor.do_send(Unsubscribe(self.id));
        Running::Stop
    }
}

impl Handler<NewPage> for PageWsActor {
    type Result = ();

    fn handle(&mut self, msg: NewPage, ctx: &mut Self::Context) -> Self::Result {
        ctx.text(msg.0)
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for PageWsActor {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(msg) => match msg {
                ws::Message::Ping(msg) => {
                    self.last_hb = Instant::now();
                    ctx.pong(&msg)
                }
                ws::Message::Pong(_) => {
                    self.last_hb = Instant::now();
                }
                ws::Message::Close(reason) => {
                    ctx.close(reason);
                    ctx.stop();
                }
                _ => {}
            },
            Err(_) => ctx.stop(),
        }
    }
}

impl PageWsActor {
    fn hb(&self, ctx: &mut <Self as actix::Actor>::Context) {
        ctx.run_interval(self.heartbeat, |act, ctx| {
            if Instant::now().duration_since(act.last_hb) > act.timeout {
                println!("Websocket Client timeout, disconnecting!");
                ctx.stop();
                return;
            }

            ctx.ping(b"");
        });
    }
}

fn setup_http_server(config: Config) -> Server {
    let heartbeat = config.heartbeat.map_or(Duration::from_secs(1), |heartbeat| {
        humantime::parse_duration(&heartbeat).expect("malformat heartbeat duration")
    });
    let timeout = config.timeout.map_or(Duration::from_secs(10), |timeout| {
        humantime::parse_duration(&timeout).expect("malformat timeout duration")
    });
    let index_html =
        fs::read_to_string(&config.template_page).expect("unable to read template page") + JS_CODE;
    let page = PageActor::load(&config.page_list, config.prev_code, config.next_code).start();

    let server = HttpServer::new({
        let page = page.clone();
        let assets = config.assets_dir.clone();
        let index_html = index_html.clone();
        move || {
            let mut app = App::new()
                .route(
                    "/",
                    web::get().to({
                        let index_html = index_html.clone();
                        move |_req: HttpRequest, _: web::Payload| index(index_html.clone())
                    }),
                )
                .route(
                    "/content/",
                    web::get().to({
                        let page = page.clone();
                        move |req, stream| content_ws(page.clone(), heartbeat, timeout, req, stream)
                    }),
                )
                .route(
                    "/hook",
                    web::delete().to({
                        let page = page.clone();
                        move |_req: HttpRequest, _: web::Payload| terminate(page.clone())
                    }),
                );
            if let Some(assets) = &assets {
                app = app.service(actix_files::Files::new("/assets", &assets));
            }
            app
        }
    })
    .bind(&config.listen)
    .expect("unable to listen")
    .run();
    println!("Listening {}", config.listen);
    server
}
